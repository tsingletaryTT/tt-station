pub mod agent_client;
pub mod catalog;
pub mod device_mesh;
pub mod discovery;
pub mod local_device;
pub mod model;
pub mod pairing;
pub mod secrets;
pub mod wol;

#[cfg(test)]
mod smoke {
    #[test]
    fn builds() {
        assert_eq!(2 + 2, 4);
    }
}
