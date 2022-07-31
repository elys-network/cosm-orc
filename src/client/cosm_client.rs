use crate::config::cfg::ChainCfg;
use anyhow::{bail, Context, Result};
use cosmos_sdk_proto::cosmos::auth::v1beta1::{
    BaseAccount, QueryAccountRequest, QueryAccountResponse,
};
use cosmos_sdk_proto::cosmos::tx::v1beta1::{SimulateRequest, SimulateResponse};
use cosmos_sdk_proto::cosmwasm::wasm::v1::{
    QuerySmartContractStateRequest, QuerySmartContractStateResponse,
};
use cosmrs::cosmwasm::{MsgExecuteContract, MsgInstantiateContract};
use cosmrs::rpc::endpoint::broadcast::tx_commit::{Response, TxResult};
use cosmrs::rpc::Client;
use cosmrs::tendermint::abci::tag::Key;
use cosmrs::tendermint::abci::{Code, Event};
use cosmrs::tx::{Fee, Msg, SignDoc, SignerInfo};
use cosmrs::{
    cosmwasm::MsgStoreCode,
    crypto::secp256k1::SigningKey,
    rpc::HttpClient,
    tx::{self},
};
use cosmrs::{AccountId, Any, Coin};
use prost::Message;
use std::future::Future;
use std::str::FromStr;

pub struct CosmClient {
    client: HttpClient,
    cfg: ChainCfg,
}

impl CosmClient {
    pub fn new(cfg: ChainCfg) -> Result<Self> {
        Ok(Self {
            client: HttpClient::new(cfg.rpc_endpoint.as_str())?,
            cfg,
        })
    }

    pub async fn store(
        &self,
        payload: Vec<u8>,
        signing_key: &SigningKey,
    ) -> Result<StoreCodeResponse> {
        let signing_public_key = signing_key.public_key();
        let sender_account_id = signing_public_key.account_id(&self.cfg.prefix).unwrap();

        let msg = MsgStoreCode {
            sender: sender_account_id.clone(),
            wasm_byte_code: payload,
            instantiate_permission: None,
        }
        .to_any()
        .unwrap();

        let tx_res = self.send_tx(msg, signing_key, sender_account_id).await?;

        let res = self
            .find_event(&tx_res, "store_code")
            .context("error storing code")?;

        let code_id = res
            .attributes
            .iter()
            .find(|a| a.key == Key::from_str("code_id").unwrap())
            .unwrap()
            .value
            .as_ref()
            .parse::<u64>()?;

        Ok(StoreCodeResponse {
            code_id,
            data: tx_res.deliver_tx,
        })
    }

    pub async fn instantiate(
        &self,
        code_id: u64,
        payload: Vec<u8>,
        signing_key: &SigningKey,
    ) -> Result<InstantiateResponse> {
        let signing_public_key = signing_key.public_key();
        let sender_account_id = signing_public_key.account_id(&self.cfg.prefix).unwrap();

        let msg = MsgInstantiateContract {
            sender: sender_account_id.clone(),
            admin: None, // TODO
            code_id,
            label: Some("cosm-orc".to_string()),
            msg: payload,
            funds: vec![], // TODO
        }
        .to_any()
        .unwrap();

        let tx_res = self.send_tx(msg, signing_key, sender_account_id).await?;

        let res = self
            .find_event(&tx_res, "instantiate")
            .context("error instantiating code")?;

        let addr = res
            .attributes
            .iter()
            .find(|a| a.key == Key::from_str("_contract_address").unwrap())
            .unwrap()
            .value
            .to_string();

        Ok(InstantiateResponse {
            address: addr,
            data: tx_res.deliver_tx,
        })
    }

    pub async fn execute(
        &self,
        address: String,
        payload: Vec<u8>,
        signing_key: &SigningKey,
    ) -> Result<ExecResponse> {
        let signing_public_key = signing_key.public_key();
        let sender_account_id = signing_public_key.account_id(&self.cfg.prefix).unwrap();

        let msg = MsgExecuteContract {
            sender: sender_account_id.clone(),
            contract: address.parse().unwrap(),
            msg: payload,
            funds: vec![], // TODO
        }
        .to_any()
        .unwrap();

        let tx_res = self.send_tx(msg, signing_key, sender_account_id).await?;

        Ok(ExecResponse {
            data: tx_res.deliver_tx,
        })
    }

    pub async fn query(&self, address: String, payload: Vec<u8>) -> Result<QueryResponse> {
        let req = QuerySmartContractStateRequest {
            address: address.parse().unwrap(),
            query_data: payload,
        };

        let mut buf = Vec::with_capacity(req.encoded_len());
        req.encode(&mut buf)?;

        let res = self
            .client
            .abci_query(
                Some("/cosmwasm.wasm.v1.Query/SmartContractState".parse()?),
                buf,
                None,
                false,
            )
            .await?;

        let res = QuerySmartContractStateResponse::decode(res.value.as_slice())?;

        // TODO: I shouldnt expose TxResult from this file, I should make my own type instead of re-exporting too
        //  * also Query is not a tx so this doesnt really make sense to conform to this type
        Ok(QueryResponse {
            data: TxResult {
                code: Code::Ok,
                data: Some(res.data.into()),
                ..Default::default()
            },
        })
    }

    async fn send_tx(&self, msg: Any, key: &SigningKey, account_id: AccountId) -> Result<Response> {
        let timeout_height = 0u16; // TODO
        let account = self.account(account_id).await?;

        let tx_body = tx::Body::new(vec![msg], "MEMO", timeout_height);

        let fee = self.simulate_gas_fee(&tx_body, &account, key).await?;

        let auth_info =
            SignerInfo::single_direct(Some(key.public_key()), account.sequence).auth_info(fee);
        let sign_doc = SignDoc::new(
            &tx_body,
            &auth_info,
            &self.cfg.chain_id.parse()?,
            account.account_number,
        )
        .unwrap();
        let tx_raw = sign_doc.sign(key).unwrap();

        let tx_commit_response = tx_raw.broadcast_commit(&self.client).await.unwrap();

        if tx_commit_response.check_tx.code.is_err() {
            bail!("check_tx failed: {:?}", tx_commit_response.check_tx)
        }
        if tx_commit_response.deliver_tx.code.is_err() {
            bail!("deliver_tx failed: {:?}", tx_commit_response.deliver_tx);
        }

        Ok(tx_commit_response)
    }

    async fn account(&self, account_id: AccountId) -> Result<BaseAccount> {
        let req = QueryAccountRequest {
            address: account_id.as_ref().into(),
        };

        let mut buf = Vec::with_capacity(req.encoded_len());
        req.encode(&mut buf)?;

        let res = self
            .client
            .abci_query(
                Some("/cosmos.auth.v1beta1.Query/Account".parse()?),
                buf,
                None,
                false,
            )
            .await?;

        let res = QueryAccountResponse::decode(res.value.as_slice())?
            .account
            .context("cannot fetch account")?;

        Ok(BaseAccount::decode(res.value.as_slice())?)
    }

    #[allow(deprecated)]
    async fn simulate_gas_fee(
        &self,
        tx: &tx::Body,
        account: &BaseAccount,
        key: &SigningKey,
    ) -> Result<Fee> {
        // TODO: support passing in the exact fee too (should be on a per process_msg() call)
        let signer_info = SignerInfo::single_direct(Some(key.public_key()), account.sequence);
        let auth_info = signer_info.auth_info(Fee::from_amount_and_gas(
            Coin {
                denom: self.cfg.denom.parse().unwrap(),
                amount: 0u64.into(),
            },
            0u64,
        ));

        let sign_doc = SignDoc::new(
            tx,
            &auth_info,
            &self.cfg.chain_id.parse().unwrap(),
            account.account_number,
        )
        .unwrap();

        let tx_raw = sign_doc.sign(key).unwrap();

        let req = SimulateRequest {
            tx: None,
            tx_bytes: tx_raw.to_bytes().unwrap(),
        };

        let mut buf = Vec::with_capacity(req.encoded_len());
        req.encode(&mut buf)?;

        let res = self
            .client
            .abci_query(
                Some("/cosmos.tx.v1beta1.Service/Simulate".parse()?),
                buf,
                None,
                false,
            )
            .await?;

        let gas_info = SimulateResponse::decode(res.value.as_slice())?
            .gas_info
            .context("error simulating tx")?;

        let gas_limit = (gas_info.gas_used as f64 * self.cfg.gas_adjustment).ceil();
        let amount = Coin {
            denom: self.cfg.denom.parse().unwrap(),
            amount: ((gas_limit * self.cfg.gas_prices).ceil() as u64).into(),
        };

        Ok(Fee::from_amount_and_gas(amount, gas_limit as u64))
    }

    fn find_event(&self, res: &Response, key_name: &str) -> Option<Event> {
        for event in &res.deliver_tx.events {
            if event.type_str == key_name {
                return Some(event.clone());
            }
        }
        None
    }
}

pub fn tokio_block<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

#[derive(Debug)]
pub struct StoreCodeResponse {
    pub code_id: u64,
    pub data: TxResult,
}

#[derive(Debug)]
pub struct InstantiateResponse {
    pub address: String,
    pub data: TxResult,
}

#[derive(Debug)]
pub struct ExecResponse {
    pub data: TxResult,
}

#[derive(Debug)]
pub struct QueryResponse {
    pub data: TxResult,
}
