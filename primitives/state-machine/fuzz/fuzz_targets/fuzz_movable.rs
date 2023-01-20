#![no_main]
use sp_state_machine::fuzz::{Ops, fuzz_movable};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: Vec<Ops>| {
	fuzz_movable(&data[..]);
});
