//! Generic tokio driver bridging any sans-IO [`Machine`] to an async [`MachineSocket`].

use std::fmt;
use std::future::Future;
use std::ops::ControlFlow;

use remora_proto::machine::{Machine, Timestamp};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};

/// Bidirectional async transport for driving a sans-IO [`Machine`].
pub trait MachineSocket<T>: Send {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Sends one outbound transmission emitted by the state machine.
    fn send(&mut self, transmit: T) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Receives the next inbound byte frame, or `Ok(None)` when the socket closes cleanly.
    fn recv(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send;
}

/// Error returned while driving a [`Machine`] over a [`MachineSocket`].
#[derive(Debug)]
#[non_exhaustive]
pub enum DriverError<ME, SE> {
    Machine(ME),
    Socket(SE),
}

impl<ME: fmt::Display, SE: fmt::Display> fmt::Display for DriverError<ME, SE> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Machine(err) => write!(f, "state machine error: {err}"),
            Self::Socket(err) => write!(f, "socket error: {err}"),
        }
    }
}

impl<ME, SE> std::error::Error for DriverError<ME, SE>
where
    ME: std::error::Error + 'static,
    SE: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Machine(err) => Some(err),
            Self::Socket(err) => Some(err),
        }
    }
}

/// Drives `machine` over `socket` until the socket closes, `event_tx` closes, or `on_command`
/// returns [`ControlFlow::Break`].
///
/// Timers from [`Machine::poll_timeout`] are awaited via [`tokio::time::sleep_until`] anchored at
/// `anchor`.
pub async fn drive_machine<M, S, C, F>(
    machine: &mut M,
    socket: &mut S,
    anchor: Instant,
    cmd_rx: &mut mpsc::Receiver<C>,
    event_tx: &mpsc::Sender<M::Event>,
    on_command: F,
) -> Result<(), DriverError<M::Error, S::Error>>
where
    M: Machine,
    M::Transmit: Send,
    M::Event: Send,
    S: MachineSocket<M::Transmit>,
    F: FnMut(&mut M, Timestamp, C) -> ControlFlow<()>,
{
    drive_machine_with(
        machine,
        socket,
        anchor,
        cmd_rx,
        event_tx,
        on_command,
        |_, _, _| {},
    )
    .await
}

/// Drives `machine` over `socket` with hooks for both inbound commands and emitted events.
pub async fn drive_machine_with<M, S, C, FC, FE>(
    machine: &mut M,
    socket: &mut S,
    anchor: Instant,
    cmd_rx: &mut mpsc::Receiver<C>,
    event_tx: &mpsc::Sender<M::Event>,
    mut on_command: FC,
    mut on_event: FE,
) -> Result<(), DriverError<M::Error, S::Error>>
where
    M: Machine,
    M::Transmit: Send,
    M::Event: Send,
    S: MachineSocket<M::Transmit>,
    FC: FnMut(&mut M, Timestamp, C) -> ControlFlow<()>,
    FE: FnMut(&mut M, Timestamp, &M::Event),
{
    let mut cmd_open = true;

    loop {
        if flush_machine(machine, socket, anchor, event_tx, &mut on_event).await?
            == ControlFlow::Break(())
        {
            return Ok(());
        }

        let deadline = machine.poll_timeout().map(|ts| anchor + ts.as_duration());
        tokio::select! {
            maybe_cmd = cmd_rx.recv(), if cmd_open => {
                match maybe_cmd {
                    Some(cmd) => {
                        let now = Timestamp::from_duration(anchor.elapsed());
                        if on_command(machine, now, cmd).is_break() {
                            let _ = flush_machine(machine, socket, anchor, event_tx, &mut on_event).await;
                            return Ok(());
                        }
                    }
                    None => {
                        cmd_open = false;
                    }
                }
            }
            inbound = socket.recv() => {
                match inbound.map_err(DriverError::Socket)? {
                    Some(frame) => {
                        let now = Timestamp::from_duration(anchor.elapsed());
                        machine
                            .handle_input(now, &frame)
                            .map_err(DriverError::Machine)?;
                    }
                    None => {
                        let _ = flush_events(machine, anchor, event_tx, &mut on_event).await;
                        return Ok(());
                    }
                }
            }
            () = sleep_until_opt(deadline) => {
                let now = Timestamp::from_duration(anchor.elapsed());
                machine.handle_timeout(now);
            }
        }
    }
}

/// Drives `machine` over `socket` without an external command channel.
pub async fn drive_until_closed<M, S>(
    machine: &mut M,
    socket: &mut S,
    anchor: Instant,
    event_tx: &mpsc::Sender<M::Event>,
) -> Result<(), DriverError<M::Error, S::Error>>
where
    M: Machine,
    M::Transmit: Send,
    M::Event: Send,
    S: MachineSocket<M::Transmit>,
{
    let (_tx, mut rx) = mpsc::channel::<()>(1);
    drive_machine(machine, socket, anchor, &mut rx, event_tx, |_, _, ()| {
        ControlFlow::Continue(())
    })
    .await
}

async fn flush_machine<M, S, FE>(
    machine: &mut M,
    socket: &mut S,
    anchor: Instant,
    event_tx: &mpsc::Sender<M::Event>,
    on_event: &mut FE,
) -> Result<ControlFlow<()>, DriverError<M::Error, S::Error>>
where
    M: Machine,
    S: MachineSocket<M::Transmit>,
    FE: FnMut(&mut M, Timestamp, &M::Event),
{
    loop {
        while let Some(tx) = machine.poll_transmit() {
            socket.send(tx).await.map_err(DriverError::Socket)?;
        }
        let mut emitted = false;
        while let Some(ev) = machine.poll_event() {
            emitted = true;
            let now = Timestamp::from_duration(anchor.elapsed());
            on_event(machine, now, &ev);
            if event_tx.send(ev).await.is_err() {
                return Ok(ControlFlow::Break(()));
            }
        }
        if !emitted {
            return Ok(ControlFlow::Continue(()));
        }
    }
}

async fn flush_events<M: Machine, FE: FnMut(&mut M, Timestamp, &M::Event)>(
    machine: &mut M,
    anchor: Instant,
    event_tx: &mpsc::Sender<M::Event>,
    on_event: &mut FE,
) -> ControlFlow<()> {
    while let Some(ev) = machine.poll_event() {
        let now = Timestamp::from_duration(anchor.elapsed());
        on_event(machine, now, &ev);
        if event_tx.send(ev).await.is_err() {
            return ControlFlow::Break(());
        }
    }
    ControlFlow::Continue(())
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(ts) => sleep_until(ts).await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::time::Duration;

    use rand::TryRng;
    use remora_proto::machine::signaling::{
        SignalingEvent, SignalingMachine, SignalingState, SignalingTransmit,
    };
    use remora_proto::signal::CONNECT;

    use super::*;

    struct DeterministicRng(u64);

    impl TryRng for DeterministicRng {
        type Error = Infallible;

        fn try_next_u32(&mut self) -> Result<u32, Infallible> {
            Ok(self.try_next_u64()? as u32)
        }
        fn try_next_u64(&mut self) -> Result<u64, Infallible> {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            Ok(self.0)
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Infallible> {
            for chunk in dest.chunks_mut(8) {
                let bytes = self.try_next_u64()?.to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
            Ok(())
        }
    }

    struct FakeSocket {
        inbound_rx: mpsc::Receiver<Option<Vec<u8>>>,
        outbound_tx: mpsc::Sender<SignalingTransmit>,
    }

    impl MachineSocket<SignalingTransmit> for FakeSocket {
        type Error = Infallible;

        async fn send(&mut self, transmit: SignalingTransmit) -> Result<(), Self::Error> {
            let is_close = matches!(transmit, SignalingTransmit::Close);
            let _ = self.outbound_tx.send(transmit).await;
            if is_close {
                self.inbound_rx.close();
            }
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<Vec<u8>>, Self::Error> {
            match self.inbound_rx.recv().await {
                Some(frame) => Ok(frame),
                None => Ok(None),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn drives_signaling_machine_handshake_and_sleep_until_timeout() {
        let anchor = Instant::now();
        let mut machine = SignalingMachine::new(DeterministicRng(7));
        machine.connect(Timestamp::ZERO);

        let (in_tx, in_rx) = mpsc::channel(16);
        let (out_tx, mut out_rx) = mpsc::channel(16);
        let (ev_tx, mut ev_rx) = mpsc::channel(16);

        let mut socket = FakeSocket {
            inbound_rx: in_rx,
            outbound_tx: out_tx,
        };

        let driver = tokio::spawn(async move {
            drive_until_closed(&mut machine, &mut socket, anchor, &ev_tx)
                .await
                .unwrap();
            machine.state()
        });

        // Server sends Engine.IO open (pingInterval=5s, pingTimeout=5s -> 10s liveness).
        in_tx
            .send(Some(
                br#"0{"sid":"s1","upgrades":[],"pingInterval":5000,"pingTimeout":5000,"maxPayload":1000000}"#.to_vec(),
            ))
            .await
            .unwrap();
        assert_eq!(
            out_rx.recv().await,
            Some(SignalingTransmit::Text(CONNECT.into()))
        );

        in_tx
            .send(Some(br#"40{"sid":"s1"}"#.to_vec()))
            .await
            .unwrap();
        assert_eq!(ev_rx.recv().await, Some(SignalingEvent::Connected));

        // Advance past the 10s liveness deadline; driver must wake on sleep_until and emit Close + TimedOut.
        tokio::time::advance(Duration::from_secs(11)).await;
        assert_eq!(out_rx.recv().await, Some(SignalingTransmit::Close));
        assert_eq!(ev_rx.recv().await, Some(SignalingEvent::TimedOut));

        let final_state = driver.await.unwrap();
        assert_eq!(final_state, SignalingState::Disconnected);
    }

    #[tokio::test(start_paused = true)]
    async fn handles_commands_and_clean_close() {
        let anchor = Instant::now();
        let mut machine = SignalingMachine::new(DeterministicRng(9));
        machine.connect(Timestamp::ZERO);

        let (in_tx, in_rx) = mpsc::channel(16);
        let (out_tx, mut out_rx) = mpsc::channel(16);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(16);

        let mut socket = FakeSocket {
            inbound_rx: in_rx,
            outbound_tx: out_tx,
        };

        let driver = tokio::spawn(async move {
            drive_machine(
                &mut machine,
                &mut socket,
                anchor,
                &mut cmd_rx,
                &ev_tx,
                |m, _now, cmd| match cmd {
                    "room_info" => {
                        m.send_room_info();
                        ControlFlow::Continue(())
                    }
                    "quit" => {
                        m.disconnect();
                        ControlFlow::Break(())
                    }
                    _ => ControlFlow::Continue(()),
                },
            )
            .await
            .unwrap();
        });

        in_tx
            .send(Some(
                br#"0{"sid":"s1","upgrades":[],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}"#.to_vec(),
            ))
            .await
            .unwrap();
        let _ = out_rx.recv().await;
        in_tx
            .send(Some(br#"40{"sid":"s1"}"#.to_vec()))
            .await
            .unwrap();
        assert_eq!(ev_rx.recv().await, Some(SignalingEvent::Connected));

        cmd_tx.send("room_info").await.unwrap();
        assert!(matches!(
            out_rx.recv().await,
            Some(SignalingTransmit::Text(ref s)) if s.contains("room_info")
        ));

        cmd_tx.send("quit").await.unwrap();
        assert_eq!(out_rx.recv().await, Some(SignalingTransmit::Close));
        assert_eq!(ev_rx.recv().await, Some(SignalingEvent::Closed));

        driver.await.unwrap();
    }
}
