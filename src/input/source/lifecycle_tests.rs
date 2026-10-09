use std::{collections::HashSet, sync::Mutex, time::Duration};

use super::{
    command::SourceCommand, fd_poll_timeout, InputError, SourceDriver, SourceInputDevice,
    SourceOutputDevice,
};
use crate::input::{capability::Capability, event::native::NativeEvent};
use tokio::sync::mpsc;

#[derive(Default)]
struct LifecycleDevice {
    transitions: Vec<&'static str>,
    filter: HashSet<Capability>,
}

impl SourceInputDevice for LifecycleDevice {
    fn poll(&mut self) -> Result<Vec<NativeEvent>, InputError> {
        Ok(vec![])
    }

    fn get_capabilities(&self) -> Result<Vec<Capability>, InputError> {
        Ok(vec![])
    }

    fn on_suspend(&mut self) {
        self.transitions.push("suspend");
    }

    fn on_resume(&mut self) {
        self.transitions.push("resume");
    }

    fn update_event_filter(&mut self, events: HashSet<Capability>) -> Result<(), InputError> {
        self.filter = events;
        Ok(())
    }
}

impl SourceOutputDevice for LifecycleDevice {
    fn stop(&mut self) -> Result<(), super::OutputError> {
        self.transitions.push("stop");
        Ok(())
    }
}

#[test]
fn queued_suspend_resume_runs_both_hooks_and_keeps_filter() {
    let (tx, mut rx) = mpsc::channel(8);
    tx.try_send(SourceCommand::Suspend).unwrap();
    tx.try_send(SourceCommand::SetEventFilter(vec![Capability::Sync]))
        .unwrap();
    tx.try_send(SourceCommand::Resume).unwrap();
    let device = Mutex::new(LifecycleDevice::default());
    let mut device = device.lock().unwrap();
    let mut filter = HashSet::new();
    let mut suspended = false;

    SourceDriver::receive_commands(&mut rx, &mut device, &mut filter, &mut suspended).unwrap();

    assert_eq!(device.transitions, ["suspend", "resume"]);
    assert!(!suspended);
    assert_eq!(device.filter, HashSet::from([Capability::Sync]));
    assert_eq!(filter, device.filter);
}

#[test]
fn repeated_lifecycle_signals_are_idempotent() {
    let (tx, mut rx) = mpsc::channel(8);
    for command in [
        SourceCommand::Resume,
        SourceCommand::Suspend,
        SourceCommand::Suspend,
        SourceCommand::Resume,
        SourceCommand::Resume,
    ] {
        tx.try_send(command).unwrap();
    }
    let device = Mutex::new(LifecycleDevice::default());
    let mut device = device.lock().unwrap();
    let mut suspended = false;

    SourceDriver::receive_commands(&mut rx, &mut device, &mut HashSet::new(), &mut suspended)
        .unwrap();

    assert_eq!(device.transitions, ["suspend", "resume"]);
    assert!(!suspended);
}

#[test]
fn suspended_source_blocks_for_commands_without_using_poll_rate() {
    let (tx, mut rx) = mpsc::channel(8);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let device = Mutex::new(LifecycleDevice::default());
        let mut device = device.lock().unwrap();
        let mut suspended = true;
        ready_tx.send(()).unwrap();
        SourceDriver::receive_commands(&mut rx, &mut device, &mut HashSet::new(), &mut suspended)
            .unwrap();
        done_tx
            .send((suspended, device.transitions.clone()))
            .unwrap();
    });

    ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    // An empty suspended queue must wait, including for upstream hidraw sources
    // configured with poll_rate = 0. It must not return into a busy polling loop.
    assert!(matches!(
        done_rx.recv_timeout(Duration::from_millis(30)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    tx.blocking_send(SourceCommand::Resume).unwrap();
    assert_eq!(
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        (false, vec!["resume"])
    );
    worker.join().unwrap();
}

#[test]
fn stop_is_processed_while_suspended() {
    let (tx, mut rx) = mpsc::channel(8);
    tx.try_send(SourceCommand::Stop).unwrap();
    let device = Mutex::new(LifecycleDevice::default());
    let mut device = device.lock().unwrap();
    let result =
        SourceDriver::receive_commands(&mut rx, &mut device, &mut HashSet::new(), &mut true);
    assert!(result.is_err());
    assert_eq!(device.transitions, ["stop"]);
}

#[test]
fn fd_wait_preserves_elapsed_time_without_truncating_or_wrapping() {
    assert_eq!(
        fd_poll_timeout(Duration::from_micros(2500), Duration::from_micros(1000)),
        2
    );
    assert_eq!(
        fd_poll_timeout(Duration::from_micros(500), Duration::ZERO),
        1
    );
    assert_eq!(
        fd_poll_timeout(Duration::from_millis(1), Duration::from_millis(2)),
        0
    );
    assert_eq!(fd_poll_timeout(Duration::ZERO, Duration::ZERO), 3);
    assert_eq!(
        fd_poll_timeout(Duration::from_secs(70), Duration::ZERO),
        u16::MAX
    );
}

#[tokio::test]
async fn source_refreshes_poll_descriptors_after_resume() {
    use std::os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    };

    struct FdDevice {
        active: Option<UnixStream>,
        replacement: Option<UnixStream>,
        observed: mpsc::UnboundedSender<RawFd>,
    }

    impl SourceInputDevice for FdDevice {
        fn poll(&mut self) -> Result<Vec<NativeEvent>, InputError> {
            assert!(
                self.active.is_some(),
                "a suspended source must not be polled"
            );
            Ok(vec![])
        }

        fn get_capabilities(&self) -> Result<Vec<Capability>, InputError> {
            Ok(vec![])
        }

        fn get_poll_fds(&self) -> Vec<RawFd> {
            let fd = self.active.as_ref().unwrap().as_raw_fd();
            self.observed.send(fd).unwrap();
            vec![fd]
        }

        fn on_suspend(&mut self) {
            self.active = None;
        }

        fn on_resume(&mut self) {
            self.active = self.replacement.take();
        }
    }

    impl SourceOutputDevice for FdDevice {}

    // Allocate both descriptors before dropping either, so the replacement
    // cannot accidentally reuse the original fd and mask a stale-fd regression.
    let (old, _old_peer) = UnixStream::pair().unwrap();
    let (new, _new_peer) = UnixStream::pair().unwrap();
    let old_fd = old.as_raw_fd();
    let new_fd = new.as_raw_fd();
    assert_ne!(old_fd, new_fd);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (composite_tx, _composite_rx) = mpsc::channel(8);
    let source = SourceDriver::new(
        composite_tx.into(),
        FdDevice {
            active: Some(old),
            replacement: Some(new),
            observed: observed_tx,
        },
        crate::udev::device::UdevDevice::default().into(),
        None,
    );
    let client = source.client();
    let task = tokio::spawn(async move { source.run().await.map_err(|error| error.to_string()) });
    let first = tokio::time::timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .unwrap();
    assert_eq!(first, Some(old_fd));
    client.suspend().await.unwrap();
    client.resume().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let fd = observed_rx
                .recv()
                .await
                .expect("source stopped before refreshing its fd");
            if fd == new_fd {
                break;
            }
        }
    })
    .await
    .expect("source must obtain the replacement fd after resume");
    client.stop().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[test]
fn source_poll_rejects_terminal_events_even_when_input_is_ready() {
    use nix::poll::PollFlags;
    for event in [PollFlags::POLLERR, PollFlags::POLLHUP, PollFlags::POLLNVAL] {
        assert!(super::check_poll_events(Some(event)).is_err());
        assert!(super::check_poll_events(Some(event | PollFlags::POLLIN)).is_err());
    }
    assert!(super::check_poll_events(None).is_err());
    assert!(super::check_poll_events(Some(PollFlags::empty())).is_ok());
    assert!(super::check_poll_events(Some(PollFlags::POLLIN)).is_ok());
}

#[test]
fn disconnected_socket_stops_fd_polling() {
    use nix::poll::{PollFd, PollFlags};
    use std::os::{fd::AsFd, unix::net::UnixStream};
    let (reader, writer) = UnixStream::pair().unwrap();
    drop(writer);
    let mut descriptors = [PollFd::new(reader.as_fd(), PollFlags::POLLIN)];
    assert!(super::poll_source_fds(&mut descriptors, 50).is_err());
}
