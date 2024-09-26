use std::sync::{Arc, Mutex};
use actix_web_lab::sse;
use futures_util::SinkExt;
use log::info;
use tokio::sync::mpsc;
use tokio::sync::mpsc::channel;

#[derive(Debug, Clone, Default)]
pub struct SseClients{
    clients: Arc<Mutex<Vec<mpsc::Sender<sse::Event>>>>
}

impl SseClients{
    pub fn new() -> Self{
        SseClients{
            clients: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn add_client(&self){
        let (tx, rx) = channel(10);
        tx.send(sse::Data::new("connected").into()).await.unwrap();
        self.clients.lock().unwrap().push(tx);
        info!("SseClients new connection found");
    }

    pub fn get_no_of_clients(&self) -> usize{
        self.clients.lock().unwrap().len()
    }

    pub async fn broadcast(&self, msg:&str){
        let clients = self.clients.lock().unwrap().clone();
        let send_futures = clients.iter().map(|client| client.send(sse::Data::new(msg).into()));
        let _ = futures_util::future::join_all(send_futures).await;
    }


}
