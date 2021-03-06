use crate::routines;
use crate::action::{DAction, AcType};
use crate::browser::BrowserCallbacks;
use crate::context::{DContext, str_hex_to_utf8, STATE_EXIT, STATE_ZERO, STATE_CURRENT, STATE_PREV};
use crate::debot_abi::DEBOT_ABI;
use ton_client_rs::{EncodedMessage, TonClient, TonError, TonErrorKind, 
    TonAddress, ResultOfLocalRun, JsonValue, Ed25519KeyPair};
use std::collections::VecDeque;
use std::io::Cursor;

fn create_client(url: &str) -> Result<TonClient, String> {
    TonClient::new_with_base_url(url)
        .map_err(|e| format!("failed to create tonclient: {}", e.to_string()))
}

pub fn load_ton_address(addr: &str) -> Result<TonAddress, String> {
    TonAddress::from_str(addr)
        .map_err(|e| format!("failed to parse address: {}", e.to_string()))
}

pub type DState = serde_json::Value;

const OPTION_ABI: u8 = 1;
const OPTION_TARGET_ABI: u8 = 2;
const OPTION_TARGET_ADDR: u8 = 4;

pub struct DEngine {
    abi: String,
    addr: TonAddress,
    ton: TonClient,
    state: DState,
    state_machine: Vec<DContext>,
    curr_state: u8,
    prev_state: u8,
    target_addr: Option<TonAddress>,
    target_abi: Option<String>,
    browser: Box<dyn BrowserCallbacks>,
}

impl DEngine {
    pub fn new(
        addr: TonAddress,
        abi: Option<String>,
        url: &str,
        browser: Box<dyn BrowserCallbacks>,
    ) -> Self {
        DEngine::new_with_client(addr, abi, create_client(url).unwrap(), browser)
    }

    pub fn new_with_client(
        addr: TonAddress,
        abi: Option<String>,
        ton: TonClient,
        browser: Box<dyn BrowserCallbacks>
    ) -> Self {
        DEngine { 
            abi: abi.unwrap_or(DEBOT_ABI.to_owned()),
            addr,
            ton,
            state: json!({}),
            state_machine: vec![],
            curr_state : STATE_EXIT,
            prev_state : STATE_ZERO,
            target_addr: None,
            target_abi: None,
            browser,
        }
    }

    pub fn fetch(&mut self) -> Result<(), String> {
        self.state_machine = self.fetch_state()?;
        self.prev_state = STATE_EXIT;
        Ok(())
    }

    fn fetch_state(&mut self) -> Result<Vec<DContext>, String> {
        self.load_state()?;
        let mut result = self.run_get("fetch")?;
        let context_vec: Vec<DContext> = serde_json::from_value(result.output["contexts"].take())
            .unwrap();
        Ok(context_vec)
    }

    pub fn start(&mut self) -> Result<(), String> {
        self.state_machine = self.fetch_state()?;

        self.switch_state(STATE_ZERO, true)
    }

    pub fn execute_action(&mut self, act: &DAction) -> Result<(), String> {
        self.handle_action(&act)
            .and_then(|_| self.switch_state(act.to, true))
            .or_else (|e| {
                self.browser.log(format!("Action failed: {}. Return to previous state.\n", e));
                self.switch_state(self.prev_state, false)
            })
    }
    
    fn handle_action(
        &mut self,
        a: &DAction,
    ) -> Result<Option<Vec<DAction>>, String> {
        match a.action_type {
            AcType::Empty => {
                debug!("empty action: {}", a.name);
                Ok(None)
            },
            AcType::RunAction => {
                debug!("run_action: {}", a.name);
                self.run_action(&a)
            },
            AcType::RunMethod => {
                debug!("run_getmethod: {}", a.func_attr().unwrap());
                let args: Option<JsonValue> = if let Some(getter) = a.args_attr() {
                    let res = self.run_debot(&getter, None)?;
                    Some(res.into())
                } else {
                    None
                };
                self.run_getmethod(&a.func_attr().unwrap(), args, &a.name)?;
                Ok(None)
            },
            AcType::SendMsg => {
                debug!("sendmsg: {}", a.name);
                let keys = if a.sign_by_user() {
                    let mut keys = Ed25519KeyPair::zero();
                    self.browser.load_key(&mut keys);
                    Some(keys)
                } else {
                    None
                };
                let args: Option<JsonValue> = if a.misc != /*empty cell*/"te6ccgEBAQEAAgAAAA==" {
                    Some(json!({ "misc": a.misc }).into())
                } else {
                    None
                };
                let result = self.run_sendmsg(&a.name, args, keys)?;
                self.browser.log(format!("Transaction succeeded."));
                if !result.is_null() {
                    self.browser.log(format!("Result: {}", result));
                }
                Ok(None)
            },
            AcType::Invoke => {
                debug!("invoke debot: run {}", a.name);
                let invoke_args = self.run_debot(&a.name, None)?;
                debug!("{}", invoke_args);
                let debot_addr = load_ton_address(invoke_args["debot"].as_str().unwrap())?;
                let debot_action: DAction = serde_json::from_value(invoke_args["action"].clone()).unwrap();
                debug!("invoke debot: {}, action name: {}", &debot_addr, debot_action.name);
                self.browser.invoke_debot(debot_addr, debot_action)?;
                Ok(None)
            },
            AcType::Print => {
                debug!("print action: {}", a.name);
                let label = if let Some(args_getter) = a.format_args() {
                    let args = if a.misc != /*empty cell*/"te6ccgEBAQEAAgAAAA==" {
                        Some(json!({"misc": a.misc}).into())
                    } else {
                        None
                    };
                    let params = self.run_debot(&args_getter, args)?;
                    routines::format_string(&a.name, &params)
                } else {
                    a.name.clone()
                };
                self.browser.log(label);
                Ok(None)
            },
            AcType::Goto => {
                debug!("goto action");
                Ok(None)
            },
            AcType::CallEngine => {
                debug!("call engine action: {}", a.name);
                let args = if let Some(args_getter) = a.args_attr() {
                    let args = self.run_debot(&args_getter, None)?;
                    args.to_string()
                } else {
                    a.desc.clone()
                };
                let keys = if a.sign_by_user() {
                    let mut keys = Ed25519KeyPair::zero();
                    self.browser.load_key(&mut keys);
                    Some(keys)
                } else {
                    None
                };                
                let res = self.call_routine(&a.name, &args, keys)?;
                let setter = a.func_attr().ok_or("routine callback is not specified".to_owned())?;
                self.run_debot(&setter, Some(json!({"arg1": res}).into()))?;
                Ok(None)
            },
            _ => {
                let err_msg = "unsupported action type".to_owned();
                self.browser.log(err_msg.clone());
                Err(err_msg)
            },
        }
    }

    fn switch_state(&mut self, mut state_to: u8, force: bool) -> Result<(), String> {
        debug!("switching to {}", state_to);
        if state_to == STATE_CURRENT {
            state_to = self.curr_state;
        }
        if state_to == STATE_PREV {
            state_to = self.prev_state;
        }
        if state_to == STATE_EXIT {
            self.browser.switch(STATE_EXIT);
        } else if state_to != self.curr_state || force {        
            let mut instant_switch = true;
            self.prev_state = self.curr_state;
            self.curr_state = state_to;
            while instant_switch {
                // TODO: restrict cyclic switches
                let jump_to_ctx = self.state_machine.iter()
                    .find(|ctx| ctx.id == state_to)
                    .map(|ctx| ctx.clone());
                if let Some(ctx) = jump_to_ctx {
                    self.browser.switch(state_to);
                    self.browser.log(ctx.desc.clone());
                    instant_switch = self.enumerate_actions(ctx)?;
                    state_to = self.curr_state;
                } else if state_to == STATE_EXIT {
                    self.browser.switch(STATE_EXIT);
                    instant_switch = false;
                } else {
                    self.browser.log(format!("Debot context #{} not found. Exit.", state_to));
                    instant_switch = false;
                }
                debug!("instant_switch = {}, state_to = {}", instant_switch, state_to);
            }
        }
        Ok(())
    }

    fn enumerate_actions(&mut self, ctx: DContext) -> Result<bool, String> {
        // find, execute and remove instant action from context.
        // if instant action returns new actions then execute them and insert into context.
        for action in &ctx.actions {
            let mut sub_actions = VecDeque::new();
            sub_actions.push_back(action.clone());
            while let Some(act) = sub_actions.pop_front() {
                if act.is_instant() {
                    if act.desc.len() != 0 {
                        self.browser.log(act.desc.clone());
                    }
                    self.handle_action(&act)?.and_then(|vec| {
                        vec.iter().for_each(|a| sub_actions.push_back(a.clone()));
                        Some(())
                    });
                    // if instant action wants to switch context then exit and do switch.
                    let to = if act.to == STATE_CURRENT {
                        self.curr_state
                    } else if act.to == STATE_PREV {
                        self.prev_state
                    } else {
                        act.to
                    };
                    if to != self.curr_state {
                        self.curr_state = act.to;
                        return Ok(true);
                    }
                } else if act.is_engine_call() {
                    self.handle_action(&act)?;
                } else {
                    self.browser.show_action(act);
                }
            }
        }
        Ok(false)
    }

    fn run_get(&mut self, name: &str) -> Result<ResultOfLocalRun, String> {
        let res = self.run(false, name, None, true, false)?;
        Ok(res)
    }

    fn run_debot(&mut self, name: &str, args: Option<JsonValue>) -> Result<serde_json::Value, String> {
        debug!("run_debot {}, args: {}", name, if args.is_some() { args.clone().unwrap() } else { json!({}).into() });
        let res = self.run(false, name, args, true, true)?;
        self.state = res.account.unwrap();
        Ok(res.output)
    }

    fn run_action(&mut self, action: &DAction) -> Result<Option<Vec<DAction>>, String> {
        let args = self.query_action_args(action)?;

        let mut output = self.run_debot(&action.name, args)?;

        let action_vec: Option<Vec<DAction>> = match output.is_null() {
            false => Some(serde_json::from_value(output["actions"].take()).unwrap()),
            true => None,
        };
        Ok(action_vec)
    }

    fn run_sendmsg(
        &mut self,
        name: &str,
        args: Option<JsonValue>,
        keys: Option<Ed25519KeyPair>,
    ) -> Result<serde_json::Value, String> {
        let result = self.run_debot(name, args)?;
        let dest = result["dest"].as_str().unwrap();
        let body = result["body"].as_str().unwrap();
        let state = result["state"].as_str();

        let state = state.map(|val| {
            base64::decode(val).map_err(|e| format!("cannot decode state: {}", e))
        }).transpose()?;

        let call_itself = load_ton_address(dest)? == self.addr;
        let abi: &str = if call_itself {
            &self.abi
        } else {
            self.target_abi.as_ref().unwrap()
        };

        let res = self.ton.contracts.decode_input_message_body(
            abi.into(),
            &base64::decode(body).unwrap(),
            true,
        ).map_err(|e| format!("failed to decode msg body: {}", e))?;

        debug!("calling {} at address {}", res.function, dest);
        debug!("args: {}", res.output);
        self.call_target(dest, abi, &res.function, res.output.into(), keys, state)
    }

    fn run_getmethod(
        &mut self,
        getmethod: &str,
        args: Option<JsonValue>,
        result_handler: &str,
    ) -> Result<serde_json::Value, String> {
        self.update_options()?;
        let result = self.run(true, getmethod, args, false, false)?;
        self.run_debot(result_handler, Some(result.output.into()))
    }

    #[allow(dead_code)]
    pub fn version(&mut self) -> Result<String, String> {
        self.run_get("getVersion").map(|res| res.output.to_string())
    }

    fn load_state(&mut self) -> Result<String, String> {
        let result = self.run(false, "getVersion", None, false, true)
            .map_err(|e| format!("failed to fetch debot state: {}", e))?;
        let name_hex = result.output["name"]
            .as_str()
            .unwrap();
        let ver_str = result.output["semver"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x");
        let name = str_hex_to_utf8(name_hex).unwrap();
        let ver = u32::from_str_radix(ver_str, 16).unwrap();
        
        self.state = result.account.unwrap();
        self.browser.log(format!("{}, version {}.{}.{}", name, ( ver >> 16) as u8, ( ver >> 8) as u8, ver as u8));
        self.update_options()?;
        Ok(result.output.to_string())
    }

    fn update_options(&mut self) -> Result<(), String> {
        let params = self.run_get("getDebotOptions")?;
        let opt_str = params.output["options"].as_str().unwrap();
        let options = u8::from_str_radix(
            opt_str.trim_start_matches("0x"),
            16,
        ).unwrap();
        if options & OPTION_ABI != 0 {
            self.abi = str_hex_to_utf8(
                params.output["debotAbi"].as_str().unwrap()
            ).ok_or("cannot convert hex string to debot abi")?;
        }
        if options & OPTION_TARGET_ABI != 0 {
            self.target_abi = str_hex_to_utf8(
                params.output["targetAbi"].as_str().unwrap()
            );
        }
        if (options & OPTION_TARGET_ADDR) != 0 {
            let addr = params.output["targetAddr"].as_str().unwrap();
            self.target_addr = Some(load_ton_address(addr)?);
        }
        Ok(())
    }

    fn query_action_args(&self, act: &DAction) -> Result<Option<JsonValue>, String> {
        let args: Option<JsonValue> = if act.misc != /*empty cell*/"te6ccgEBAQEAAgAAAA==" {
            Some(json!({ "misc": act.misc }).into())
        } else {
            let abi_json: serde_json::Value = serde_json::from_str(&self.abi).unwrap();
            let functions = abi_json["functions"].as_array().unwrap();
            let func = functions.iter().find(|f| f["name"].as_str().unwrap() == act.name)
                .ok_or(format!("action not found"))?;
            let arguments = func["inputs"].as_array().unwrap();
            let mut args_json = json!({});
            for arg in arguments {
                let arg_name = arg["name"].as_str().unwrap();
                let prefix = "".to_owned();
                let mut value = String::new();
                self.browser.input(&prefix, &mut value);
                if arg["type"].as_str().unwrap() == "bytes" {
                    value = hex::encode(value.as_bytes());
                }
                args_json[arg_name] = json!(&value);
            }
            Some(args_json.into())
        };
        Ok(args)
    }

    fn get_target(&self) -> Result<(&TonAddress, &String), String> {
        let addr = self.target_addr.as_ref().ok_or(
            format!("target address is undefined")
        )?;
        let abi = self.target_abi.as_ref().ok_or(
            format!("target abi is undefined")
        )?;
        Ok((addr, abi))
    }

    fn run(
        &self,
        is_target: bool,
        func: &str,
        args: Option<JsonValue>,
        with_state: bool,
        emulate_real_txn: bool
    ) -> Result<ResultOfLocalRun, String> {
        let (addr, abi) = if is_target {
            self.get_target()?
        } else {
            (&self.addr, &self.abi)
        };
        let abi: &str = abi;
        debug!("running {}, addr {}, state = {}", func, &addr, with_state);
        self.ton.contracts.run_local(
            addr,
            if with_state { Some(self.state.clone().into()) } else { None },
            abi.into(),
            func,
            None,
            args.unwrap_or(json!({}).into()),
            None,
            None,
            emulate_real_txn
        )
        .map_err(|e| {
            error!("{}", e);
            self.handle_sdk_err(e)
        })
    }

    fn call_target(
        &self,
        dest: &str,
        abi: &str,
        func: &str,
        args: JsonValue,
        keys: Option<Ed25519KeyPair>,
        state: Option<Vec<u8>>,
    ) -> Result<serde_json::Value, String > {
        let addr = load_ton_address(dest)?;

        let msg = self.ton.contracts.create_run_message(
            &addr,
            abi.into(),
            func,
            None,
            args,
            keys.as_ref(),
            None,
        )
        .map_err(|e| {
            error!("failed to create message: {}", e);
            format!("failed to create message")
        })?;

        let msg = pack_state(msg, state)?;

        self.browser.log(format!("sending message {}", msg.message_id));
        let res = self.ton.contracts.process_message(msg, Some(abi.into()), Some(func), false)
            .map_err(|e| {
                error!("{}", e);
                self.handle_sdk_err(e)
            })
            .map(|res| res.output)?;

        Ok(res)
    }

    fn call_routine(
        &self,
        name: &str,
        args: &str,
        keypair: Option<Ed25519KeyPair>
    ) -> Result<String, String> {
        routines::call_routine(&self.ton, name, args, keypair)
    }

    fn handle_sdk_err(&self, err: TonError) -> String {
        match err {
            TonError(TonErrorKind::InnerSdkError(inn), _) => {
                if inn.message.contains("Wrong data format") {
                    // when debot's function argument has invalid format
                    "invalid parameter".to_owned()
                } else if inn.code == 3025 {
                    // when debot function throws an exception
                    if let Some(err) = inn.data["exit_code"].as_i64() {
                        self.run(
                            false,
                            "getErrorDescription",
                            Some(json!({"error": err}).into()),
                            true,
                            false,
                        ).ok().and_then(|res| {
                            res.output["desc"].as_str()
                                .and_then(|hex| {
                                    hex::decode(&hex).ok()
                                        .and_then(|vec| String::from_utf8(vec).ok())
                                })
                        }).unwrap_or(inn.message)
                    } else {
                        inn.message
                    }
                } else {
                    
                    inn.message
                }
            },
            _ => format!("{}", err)
        }
    }
}

fn pack_state(mut msg: EncodedMessage, state: Option<Vec<u8>>) -> Result<EncodedMessage, String> {
    if state.is_some() {
        let mut buff = Cursor::new(state.unwrap());
        let image = ton_sdk::ContractImage::from_state_init(&mut buff)
            .map_err(|e| format!("unable to build contract image: {}", e))?;
        let state_init = image.state_init();
        let mut raw_msg = ton_sdk::Contract::deserialize_message(&msg.message_body[..])
            .map_err(|e| format!("cannot deserialize buffer to msg: {}", e))?;
        raw_msg.set_state_init(state_init);
        let (msg_bytes, message_id) = ton_sdk::Contract::serialize_message(&raw_msg)
            .map_err(|e| format!("cannot serialize msg with state: {}", e))?;
        msg.message_body = msg_bytes;
        msg.message_id = message_id.to_string();
    }
    Ok(msg)
}