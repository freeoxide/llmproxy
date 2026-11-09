//! The in-process RESP2 test server now lives in the crate behind the
//! `test-server` feature (`llm_proxy_store_redis::test_server`), so
//! downstream integration tests can boot it too. This shim keeps the
//! crate's own integration tests reading as they always have.

pub(crate) use llm_proxy_store_redis::test_server::TestRedis;
