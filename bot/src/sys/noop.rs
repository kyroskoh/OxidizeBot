use failure::Error;
use futures::channel::oneshot;

#[derive(Clone)]
pub struct System;

impl System {
    pub async fn wait_for_shutdown(&self) -> Result<(), oneshot::Canceled> {
        future::empty().await;
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        true
    }

    pub fn join(&self) -> Result<(), Error> {
        Ok(())
    }
}

pub fn setup() -> Result<System, Error> {
    Ok(System)
}
