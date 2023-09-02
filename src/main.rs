use std::env;

use dotenv::dotenv;
use rlp::{Rlp, RlpStream};
use tiny_keccak::{Hasher, Keccak};
use web3::transports::Http;
use web3::types::{Block, BlockNumber, H160, H2048, H256, U256, U64};

const BLOCK_HEADER_RLP_BYTES: usize = 590;
const PROOF_BYTES_LEN: usize = 532;
const ACCOUNT_PROOF_MAX_DEPTH: usize = 10;
const STORAGE_PROOF_MAX_DEPTH: usize = 9;

fn bloom_to_bytes(bloom_option: Option<H2048>) -> Vec<u8> {
    match bloom_option {
        Some(bloom) => bloom.as_bytes().to_vec(),
        None => {
            vec![]
        }
    }
}

fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut keccak = Keccak::v256();
    let mut result = [0u8; 32];
    keccak.update(data);
    keccak.finalize(&mut result);
    result
}

fn split_rlp_by_state_root(
    rlp_data: &[u8],
    state_root: &[u8],
) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    if let Some(start) = find_subarray(&rlp_data, &state_root) {
        let rlp_head = rlp_data[..start].to_vec();
        let state_root_bytes = rlp_data[start..start + 32].to_vec();
        let rlp_tail = rlp_data[start + 32..].to_vec();

        Some((rlp_head, state_root_bytes, rlp_tail))
    } else {
        None
    }
}

fn find_subarray(array: &[u8], subarray: &[u8]) -> Option<usize> {
    array
        .windows(subarray.len())
        .position(|window| window == subarray)
}

fn rlp_encode_block(block: &Block<H256>) -> Vec<u8> {
    let mut rlp_stream = RlpStream::new();

    let mut num_items = 15;

    if let Some(_) = block.base_fee_per_gas {
        num_items += 1;
    }

    rlp_stream
        .begin_list(num_items)
        .append(&block.parent_hash)
        .append(&block.uncles_hash)
        .append(&block.author)
        .append(&block.state_root)
        .append(&block.transactions_root)
        .append(&block.receipts_root)
        .append(&bloom_to_bytes(block.logs_bloom))
        .append(&block.difficulty)
        .append(&block.number.unwrap_or_default())
        .append(&block.gas_limit)
        .append(&block.gas_used)
        .append(&block.timestamp)
        .append(&block.extra_data.0)
        .append(&block.mix_hash.unwrap_or_default())
        .append(&block.nonce.unwrap_or_default());

    if let Some(base_fee_per_gas) = block.base_fee_per_gas {
        if base_fee_per_gas != U256::zero() {
            rlp_stream.append(&base_fee_per_gas);
        }
    }

    let rlp_data = rlp_stream.as_raw();
    let hash = keccak256(&rlp_data);
    assert_eq!(
        block.hash.unwrap(),
        hash.into(),
        "Rlp_encode_block: Block hash mismatch!"
    );

    rlp_stream.out().to_vec()
}

#[tokio::main]
async fn main() -> web3::Result<()> {
    dotenv().ok();
    let args: Vec<String> = env::args().collect();
    if args.len() == 1 {
        panic!("No arguments passed.");
    }

    let provider_url = env::var("MAINNET_RPC").unwrap();
    let http = Http::new(&provider_url)?;
    let web3 = web3::Web3::new(http);
    let block_number = env::var("BLOCK_NUMBER").unwrap();
    let block_number = BlockNumber::Number(U64::from_str_radix(&block_number, 10).unwrap());
    let block = web3
        .eth()
        .block(web3::types::BlockId::Number(block_number))
        .await?;

    if let Some(block) = block {
        let target_account: H160 =
            H160::from_slice(&hex::decode(env::var("TARGET_ACCOUNT").unwrap()).unwrap());
        let slot: H256 = H256::from_slice(&hex::decode(env::var("STORAGE_SLOT").unwrap()).unwrap());
        let slot_u256 = U256::from_big_endian(&slot.0);

        let mut rlp_encoded_block = rlp_encode_block(&block);

        let rlp = Rlp::new(&rlp_encoded_block);

        let state_root = match rlp.at(3) {
            Ok(item) => item
                .data()
                .map_err(|e| web3::Error::Decoder(format!("Failed to decode: {:?}", e)))?
                .to_vec(),
            Err(_) => {
                return Err(web3::Error::Decoder(
                    "Failed to decode RLP at index 3".to_string(),
                ))
            }
        };

        let (rlp_head_bytes, _, rlp_tail_bytes) =
            split_rlp_by_state_root(&rlp_encoded_block, state_root.as_slice())
                .expect("Failed to split RLP data");

        let hash = keccak256(&rlp_encoded_block);
        assert_eq!(
            block.hash.unwrap(),
            hash.into(),
            "Verification: Block hash mismatch!"
        );

        while rlp_encoded_block.len() < BLOCK_HEADER_RLP_BYTES {
            rlp_encoded_block.push(0);
        }

        let proof = web3
            .eth()
            .proof(target_account, vec![slot_u256], Some(block_number.into()))
            .await?;

        let unwrapped = &proof.unwrap_or_default();

        let mut account_value_rlp_stream = RlpStream::new();
        account_value_rlp_stream
            .begin_list(4)
            .append(&unwrapped.nonce)
            .append(&unwrapped.balance)
            .append(&unwrapped.storage_hash)
            .append(&unwrapped.code_hash);

        let mut account_proof: Vec<Vec<u8>> = Vec::new();

        for proof in &unwrapped.account_proof {
            let mut raw = proof.0.clone();
            while raw.len() < PROOF_BYTES_LEN {
                raw.push(0);
            }
            account_proof.push(raw);
        }

        while account_proof.len() < ACCOUNT_PROOF_MAX_DEPTH {
            account_proof.push(vec![0; PROOF_BYTES_LEN]);
        }

        let mut account_proof_flat_vec = Vec::new();
        for inner_vec in account_proof {
            for item in inner_vec {
                account_proof_flat_vec.push(item);
            }
        }

        let mut storage_proof: Vec<Vec<u8>> = Vec::new();

        for proof in &unwrapped.storage_proof[0].proof {
            let mut raw = proof.0.clone();
            while raw.len() < PROOF_BYTES_LEN {
                raw.push(0);
            }
            storage_proof.push(raw);
        }

        while storage_proof.len() < STORAGE_PROOF_MAX_DEPTH {
            storage_proof.push(vec![0; PROOF_BYTES_LEN]);
        }

        let mut storage_proof_flat_vec = Vec::new();
        for inner_vec in storage_proof {
            for item in inner_vec {
                storage_proof_flat_vec.push(item);
            }
        }

        let storage_key = U256::from(&unwrapped.storage_proof[0].key);
        let storage_value = U256::from(&unwrapped.storage_proof[0].value);
        let mut storage_key_bytes = [0u8; 32];
        let mut storage_value_bytes = [0u8; 32];
        storage_key.to_big_endian(&mut storage_key_bytes);
        storage_value.to_big_endian(&mut storage_value_bytes);

        if &args[1] == "gen_prove_params" {
            // Output
            println!("block_hash = {:?}", block.hash.unwrap().as_bytes());
            println!("account_key = {:?}", target_account.as_bytes());
            println!("account_value = {:?}", account_value_rlp_stream.as_raw());
            println!("storage_key = {:?}", storage_key_bytes);
            println!("storage_value = {:?}", storage_value_bytes);
            println!("block_header_rlp = {:?}", rlp_encoded_block);
            println!("block_header_rlp_head_len = {:?}", rlp_head_bytes.len());
            println!("block_header_rlp_tail_len = {:?}", rlp_tail_bytes.len());
            println!("storage_root = {:?}", &unwrapped.storage_hash.as_bytes());
            println!("account_proof = {:?}", account_proof_flat_vec);
            println!("storage_proof = {:?}", storage_proof_flat_vec);
            println!("account_proof_depth = {:?}", &unwrapped.account_proof.len());
            println!(
                "storage_proof_depth = {:?}",
                &unwrapped.storage_proof[0].proof.len()
            );
        } else if &args[1] == "gen_verify_params" {
            println!("account_key = {:?}", target_account.as_bytes());
            println!("account_value = {:?}", account_value_rlp_stream.as_raw());
            println!("block_hash = {:?}", block.hash.unwrap().as_bytes());
            println!("storage_key = {:?}", storage_key_bytes);
            println!("storage_value = {:?}", storage_value_bytes);
        } else {
            panic!("Invalid command!");
        }
    } else {
        eprintln!("Block not found!");
    }

    Ok(())
}
