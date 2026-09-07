//! Concrete TCP/UDP target adapter for the generic Home product.
use flowsplice_core::protocol::Service;
use flowsplice_transport::{BoxStream, DatagramIo, IoFuture, ServicePeer, ServiceProvider};
use std::sync::Arc;
use tokio::net::{TcpStream, UdpSocket};

pub struct NetworkTargets;
struct NetworkDatagram(UdpSocket);
impl DatagramIo for NetworkDatagram {
    fn send<'a>(&'a self, bytes: &'a [u8]) -> IoFuture<'a, ()> {
        Box::pin(async move {
            self.0.send(bytes).await?;
            Ok(())
        })
    }
    fn recv(&self) -> IoFuture<'_, Vec<u8>> {
        Box::pin(async move {
            let mut bytes = vec![0; 65_507];
            let count = self.0.recv(&mut bytes).await?;
            bytes.truncate(count);
            Ok(bytes)
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
            Ok(Arc::new(NetworkDatagram(socket)) as Arc<dyn DatagramIo>)
        })
    }
}
