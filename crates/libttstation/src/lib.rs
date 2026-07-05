pub mod agent_client;
pub mod catalog;
pub mod discovery;
pub mod model;
pub mod pairing;
pub mod secrets;

#[cfg(test)]
mod smoke {
    #[test]
    fn builds() {
        assert_eq!(2 + 2, 4);
    }
}
