use cosmwasm_std::{from_slice, Binary, ContractResult, Empty, Querier, QueryRequest, WasmQuery, SystemResult, SystemError};

pub type MockWasmSmartHandler = fn(String, Binary) -> SystemResult<ContractResult<Binary>>;

pub struct MockContractQuerier {
  pub wasm_smart_handler: MockWasmSmartHandler,
}

impl Querier for MockContractQuerier {
    fn raw_query(&self, bin_request: &[u8]) -> SystemResult<ContractResult<Binary>> { 
        let request: QueryRequest<Empty> = match from_slice(bin_request) {
            Ok(v) => v,
            Err(e) => {
                return SystemResult::Err(SystemError::InvalidRequest {
                    error: format!("Parsing query request: {}", e),
                    request: bin_request.into(),
                })
            }
        };

        return match request {
            QueryRequest::Wasm(msg) => {
                match msg {
                    WasmQuery::Smart { contract_addr, msg } => (self.wasm_smart_handler)(contract_addr, msg),
                    _ => SystemResult::Err(SystemError::UnsupportedRequest {
                        kind: String::from("only support smart wasm"),
                    }),
                }
            },
            _ => SystemResult::Err(SystemError::UnsupportedRequest {
                kind: String::from("only support wasm"),
            }),
        }
    }
}