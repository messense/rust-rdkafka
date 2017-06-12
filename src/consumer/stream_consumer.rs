//! Stream-based consumer implementation.
use futures::{Future, Poll, Sink, Stream};
use futures::sync::mpsc;
use rdsys::types::*;
use rdsys;

use config::{FromClientConfig, FromClientConfigAndContext, ClientConfig};
use consumer::base_consumer::BaseConsumer;
use consumer::{Consumer, ConsumerContext, EmptyConsumerContext};
use error::{KafkaError, KafkaResult};
use message::Message;
use util::duration_to_millis;

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;


/// A Consumer with an associated polling thread. This consumer doesn't need to
/// be polled and it will return all consumed messages as a `Stream`.
/// Due to the asynchronous nature of the stream, some messages might be consumed by the consumer
/// without being processed on the other end of the stream. If auto commit is used, it might cause
/// message loss after consumer restart. Manual offset storing should be used, see the `store_offset`
/// function on `Consumer`.
#[must_use = "Consumer polling thread will stop immediately if unused"]
pub struct StreamConsumer<C: ConsumerContext + 'static> {
    consumer: Arc<BaseConsumer<C>>,
    should_stop: Arc<AtomicBool>,
    handle: Cell<Option<JoinHandle<()>>>,
}

impl<C: ConsumerContext> Consumer<C> for StreamConsumer<C> {
    fn get_base_consumer(&self) -> &BaseConsumer<C> {
        Arc::as_ref(&self.consumer)
    }
}

impl FromClientConfig for StreamConsumer<EmptyConsumerContext> {
    fn from_config(config: &ClientConfig) -> KafkaResult<StreamConsumer<EmptyConsumerContext>> {
        StreamConsumer::from_config_and_context(config, EmptyConsumerContext)
    }
}

/// Creates a new `Consumer` starting from a `ClientConfig`.
impl<C: ConsumerContext> FromClientConfigAndContext<C> for StreamConsumer<C> {
    fn from_config_and_context(config: &ClientConfig, context: C) -> KafkaResult<StreamConsumer<C>> {
        let stream_consumer = StreamConsumer {
            consumer: Arc::new(BaseConsumer::from_config_and_context(config, context)?),
            should_stop: Arc::new(AtomicBool::new(false)),
            handle: Cell::new(None),
        };
        Ok(stream_consumer)
    }
}

struct PolledPtr {
    message_ptr: Option<*mut RDKafkaMessage>,
}

impl PolledPtr {
    fn new(message_ptr: *mut RDKafkaMessage) -> PolledPtr {
        trace!("New polled ptr {:?}", message_ptr);
        PolledPtr {
            message_ptr: Some(message_ptr)
        }
    }

    fn into_message_of<'a, T>(mut self, message_container: &'a T) -> Message<'a> {
        Message::new(self.message_ptr.take().unwrap(), message_container)
    }
}

impl Drop for PolledPtr {
    fn drop(&mut self) {
        if let Some(ptr) = self.message_ptr {
            trace!("Destroy PolledPtr {:?}", ptr);
            unsafe { rdsys::rd_kafka_message_destroy(ptr) };
        }
    }
}

// TODO: add docs
unsafe impl Send for PolledPtr {}


/// A Stream of Kafka messages. It can be used to receive messages as they are received.
pub struct MessageStream<'a, C: ConsumerContext + 'static> {
    consumer: &'a StreamConsumer<C>,
    receiver: mpsc::Receiver<KafkaResult<PolledPtr>>,
}

impl<'a, C: ConsumerContext + 'static> MessageStream<'a, C> {
    fn new(consumer: &'a StreamConsumer<C>, receiver: mpsc::Receiver<KafkaResult<PolledPtr>>) -> MessageStream<'a, C> {
        MessageStream {
            consumer: consumer,
            receiver: receiver,
        }
    }
}

impl<'a, C: ConsumerContext + 'a> Stream for MessageStream<'a, C> {
    type Item = KafkaResult<Message<'a>>;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.receiver.poll() {
            Ok(async) => Ok(async.map(|option|
                option.map(|result|
                    result.map(|polled_ptr| polled_ptr.into_message_of(self.consumer))))),
            Err(e) => Err(e),
        }
    }
}

impl<C: ConsumerContext> StreamConsumer<C> {
    /// Starts the StreamConsumer with default configuration (100ms polling interval and no
    /// `NoMessageReceived` notifications).
    pub fn start(&self) -> MessageStream<C> {
        self.start_with(Duration::from_millis(100), false)
    }

    /// Starts the StreamConsumer with the specified poll interval. Additionally, if
    /// `no_message_error` is set to true, it will return an error of type
    /// `KafkaError::NoMessageReceived` every time the poll interval is reached and no message
    /// has been received.
    pub fn start_with(&self, poll_interval: Duration, no_message_error: bool) -> MessageStream<C> {
        let (sender, receiver) = mpsc::channel(0);
        let consumer = self.consumer.clone();
        let should_stop = self.should_stop.clone();
        let handle = thread::Builder::new()
            .name("poll".to_string())
            .spawn(move || {
                poll_loop(consumer, sender, should_stop, poll_interval, no_message_error);
            })
            .expect("Failed to start polling thread");
        self.handle.set(Some(handle));
        MessageStream::new(self, receiver)
    }

    /// Stops the StreamConsumer, blocking the caller until the internal consumer
    /// has been stopped.
    pub fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            trace!("Stopping polling");
            self.should_stop.store(true, Ordering::Relaxed);
            trace!("Waiting for polling thread termination");
            match handle.join() {
                Ok(()) => trace!("Polling stopped"),
                Err(e) => warn!("Failure while terminating thread: {:?}", e),
            };
        }
    }
}

impl<C: ConsumerContext> Drop for StreamConsumer<C> {
    fn drop(&mut self) {
        trace!("Destroy StreamConsumer");
        // The polling thread must be fully stopped before we can proceed with the actual drop,,
        // otherwise it might consume from a destroyed consumer.
        self.stop();
    }
}

/// Internal consumer loop.
fn poll_loop<C: ConsumerContext>(
    consumer: Arc<BaseConsumer<C>>,
    sender: mpsc::Sender<KafkaResult<PolledPtr>>,
    should_stop: Arc<AtomicBool>,
    poll_interval: Duration,
    no_message_error: bool,
) {
    trace!("Polling thread loop started");
    let mut curr_sender = sender;
    let poll_interval_ms = duration_to_millis(poll_interval) as i32;
    while !should_stop.load(Ordering::Relaxed) {
        trace!("Polling base consumer");
        let future_sender = match consumer.poll_raw(poll_interval_ms) {
            Ok(None) => {
                if no_message_error {
                    curr_sender.send(Err(KafkaError::NoMessageReceived))
                } else {
                    continue // TODO: check stream closed
                }
            },
            Ok(Some(m_ptr)) => curr_sender.send(Ok(PolledPtr::new(m_ptr))),
            Err(e) => curr_sender.send(Err(e)),
        };
        match future_sender.wait() {
            Ok(new_sender) => curr_sender = new_sender,
            Err(e) => {
                debug!("Sender not available: {:?}", e);
                break;
            }
        };
    }
    trace!("Polling thread loop terminated");
}
