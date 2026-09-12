//! Concrete TCP/UDP target adapter for the generic Home product.
use flowsplice_core::protocol::Service;
use flowsplice_transport::{BoxStream, DatagramIo, IoFuture, ServicePeer, ServiceProvider};
use std::sync::Arc;
use tokio::net::{TcpStream, UdpSocket};

pub struct NetworkTargets;
struct NetworkDatagram {
    socket: UdpSocket,
    receive_buffer: tokio::sync::Mutex<Vec<u8>>,
}
impl DatagramIo for NetworkDatagram {
    fn send<'a>(&'a self, bytes: &'a [u8]) -> IoFuture<'a, ()> {
        Box::pin(async move {
            self.socket.send(bytes).await?;
            Ok(())
        })
    }
    fn recv(&self) -> IoFuture<'_, Vec<u8>> {
        Box::pin(async move {
            let mut bytes = self.receive_buffer.lock().await;
            let count = self.socket.recv(&mut bytes).await?;
            Ok(bytes[..count].to_vec())
        })
    }
}
impl ServiceProvider for NetworkTargets {
    fn connect_tcp<'a>(
        &'a self,
        service: &'a Service,
        _peer: ServicePeer,
    ) -> IoFuture<'a, BoxStream> {
        Box::pin(async move {
            let stream = TcpStream::connect(&service.target).await?;
            stream.set_nodelay(true)?;
            Ok(Box::new(stream) as BoxStream)
        })
    }
    fn connect_udp<'a>(
        &'a self,
        service: &'a Service,
        _peer: ServicePeer,
    ) -> IoFuture<'a, Arc<dyn DatagramIo>> {
        Box::pin(async move {
            let socket = UdpSocket::bind("0.0.0.0:0").await?;
            socket.connect(&service.target).await?;
            Ok(Arc::new(NetworkDatagram {
                socket,
                receive_buffer: tokio::sync::Mutex::new(vec![0; 65_507]),
            }) as Arc<dyn DatagramIo>)
        })
    }
}
