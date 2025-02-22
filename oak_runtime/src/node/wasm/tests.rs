//
// Copyright 2020 The Project Oak Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

use super::*;
use crate::{permissions::PermissionsConfiguration, RuntimeProxy, SecureServerConfiguration};
use maplit::hashmap;
use oak_abi::{
    label::Label,
    proto::oak::application::{
        node_configuration::ConfigType, ApplicationConfiguration, WebAssemblyConfiguration,
    },
};
use oak_sign::{get_sha256_hex, SignatureBundle};
use std::fs::read;
use wat::parse_str;

fn start_node(
    wasm_module: Vec<u8>,
    entrypoint_name: &str,
    signatures: &[SignatureBundle],
) -> Result<(), OakStatus> {
    crate::tests::init_logging();
    let module_name = "oak_module";
    let module_hash = get_sha256_hex(wasm_module.as_ref());
    let application_configuration = ApplicationConfiguration {
        wasm_modules: hashmap! { module_name.to_string() => wasm_module },
        initial_node_configuration: None,
        module_signatures: vec![],
    };
    for signature in signatures.iter() {
        signature.verify().map_err(|error| {
            error!("Wasm module signature verification failed: {:?}", error);
            OakStatus::ErrInvalidArgs
        })?;
        if module_hash != hex::encode(&signature.hash) {
            error!("Incorrect Wasm module signature hash");
            return Err(OakStatus::ErrInvalidArgs);
        }
    }
    let permissions = PermissionsConfiguration {
        allow_grpc_server_nodes: true,
        ..Default::default()
    };
    let signature_table = SignatureTable {
        values: hashmap! { module_hash => signatures.to_vec() },
    };
    let proxy = RuntimeProxy::create_runtime(
        &application_configuration,
        &permissions,
        &SecureServerConfiguration::default(),
        &signature_table,
        None,
    );
    let (_write_handle, read_handle) = proxy.channel_create("", &Label::public_untrusted())?;

    let result = proxy.node_create(
        "test",
        &NodeConfiguration {
            config_type: Some(ConfigType::WasmConfig(WebAssemblyConfiguration {
                wasm_module_name: module_name.to_string(),
                wasm_entrypoint_name: entrypoint_name.to_string(),
            })),
        },
        &Label::public_untrusted(),
        read_handle,
    );

    proxy
        .channel_close(read_handle)
        .expect("could not close channel");

    // Ensure that the runtime can terminate correctly, regardless of what the node does.
    proxy.runtime.stop();

    result
}

fn load_signature(signature_path: &str) -> SignatureBundle {
    SignatureBundle::from_pem_file(signature_path).expect("Couldn't parse signature")
}

#[test]
fn wasm_starting_module_without_content_fails() {
    // Loads an empty module that does not have the necessary entrypoint, so it should fail
    // immediately.
    let binary = read("testdata/empty.wasm").expect("Couldn't read Wasm file");

    // An empty module is equivalent to: [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]
    // From https://docs.rs/wasmi/0.6.2/wasmi/struct.Module.html#method.from_buffer:
    // Minimal module:
    // \0asm - magic
    // 0x01 - version (in little-endian)
    assert_eq!(binary, vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);
    let result = start_node(binary, "oak_main", vec![].as_ref());
    assert_eq!(Some(OakStatus::ErrInvalidArgs), result.err());
}

#[test]
fn wasm_starting_minimal_module_succeeds() {
    let binary = read("testdata/minimal.wasm").expect("Couldn't read Wasm file");
    let result = start_node(binary, "oak_main", vec![].as_ref());
    assert!(result.is_ok());
}

#[test]
fn wasm_starting_module_missing_an_export_fails() {
    let binary = read("testdata/missing.wasm").expect("Couldn't read Wasm file");
    let result = start_node(binary, "oak_main", vec![].as_ref());
    assert_eq!(Some(OakStatus::ErrInvalidArgs), result.err());
}

#[test]
fn wasm_starting_module_with_wrong_export_fails() {
    let binary = read("testdata/minimal.wasm").expect("Couldn't read Wasm file");
    let result = start_node(binary, "oak_other_main", vec![].as_ref());
    assert_eq!(Some(OakStatus::ErrInvalidArgs), result.err());
}

#[test]
fn wasm_starting_module_with_wrong_signature_fails() {
    let binary = read("testdata/wrong.wasm").expect("Couldn't read Wasm file");
    let result = start_node(binary, "oak_main", vec![].as_ref());
    assert_eq!(Some(OakStatus::ErrInvalidArgs), result.err());
}

#[test]
fn wasm_starting_module_with_wrong_signature_2_fails() {
    // As a source of inspiration for writing tests in future, this test intentionally parses
    // the module from a string literal as opposed to loading from file.

    // Wrong signature: oak_main does not take any parameters
    let wat = r#"
    (module
        (func $oak_main)
        (memory (;0;) 18)
        (export "memory" (memory 0))
        (export "oak_main" (func $oak_main)))
    "#;
    let binary = parse_str(wat).unwrap();
    let result = start_node(binary, "oak_main", vec![].as_ref());
    assert_eq!(Some(OakStatus::ErrInvalidArgs), result.err());
}

#[test]
fn wasm_starting_module_with_wrong_signature_3_fails() {
    // Wrong signature: oak_main has the correct input parameter, but returns i32
    let wat = r#"
    (module
        (type (;0;) (func (param i64) (result i32)))
        (func $oak_main (type 0)
          i32.const 42)
        (memory (;0;) 18)
        (export "memory" (memory 0))
        (export "oak_main" (func $oak_main)))
    "#;
    let binary = parse_str(wat).unwrap();
    let result = start_node(binary, "oak_main", vec![].as_ref());
    assert_eq!(Some(OakStatus::ErrInvalidArgs), result.err());
}

#[test]
fn wasm_verify_module_signature_succeeds() {
    let binary = read("testdata/minimal.wasm").expect("Couldn't read Wasm file");
    let signature = load_signature("testdata/minimal.sign");
    let result = start_node(binary, "oak_main", vec![signature].as_ref());
    assert!(result.is_ok());
}

#[test]
fn wasm_verify_module_signature_fails() {
    let binary = read("testdata/minimal.wasm").expect("Couldn't read Wasm file");
    let signature = load_signature("testdata/wrong.sign");
    let result = start_node(binary, "oak_main", vec![signature].as_ref());
    assert_eq!(Some(OakStatus::ErrInvalidArgs), result.err());
}
