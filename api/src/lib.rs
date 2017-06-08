// Copyright 2016 The Grin Developers
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

extern crate grin_core as core;
extern crate grin_chain as chain;
extern crate grin_util as util;
extern crate secp256k1zkp as secp;

extern crate hyper;
#[macro_use]
extern crate log;
extern crate iron;
extern crate router;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

pub mod client;
mod endpoints;
mod rest;

pub use endpoints::start_rest_apis;
pub use rest::*;
