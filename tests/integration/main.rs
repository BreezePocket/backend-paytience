//! End-to-end tests: the real server on 127.0.0.1:0, fake MMs over WebSockets, and a
//! local mock JSON-RPC server. Hermetic: nothing leaves localhost.

mod harness;

mod admin;
mod auth;
mod faucet;
mod http;
mod market;
mod ratelimit;
mod rfq;
mod sign;
