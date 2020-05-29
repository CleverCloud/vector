use crate::{
    buffers::Acker,
    event::{self, Event},
    runtime::TaskExecutor,
    topology::config::{DataType, SinkConfig, SinkContext, SinkDescription},
};
use futures01::{
    future, stream::FuturesUnordered, Async, AsyncSink, Future, Poll, Sink, StartSend, Stream,
};
use futures::{compat::Compat, lock::Mutex};
use pulsar::{
    Error as PulsarError, proto::CommandSendReceipt, Authentication, ProducerOptions, Pulsar, TopicProducer, TokioExecutor,
};
use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};
use std::{collections::HashSet, sync::{Arc, mpsc::channel}};

type MetadataFuture<F, M> = future::Join<F, future::FutureResult<M, <F as Future>::Error>>;

#[derive(Debug, Snafu)]
enum BuildError {
    #[snafu(display("creating pulsar producer failed: {}", source))]
    CreatePulsarSink { source: pulsar::Error },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PulsarSinkConfig {
    address: String,
    topic: String,
    encoding: Encoding,
    auth: Option<AuthConfig>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuthConfig {
    name: String,  // "token"
    token: String, // <jwt token>
}

#[derive(Deserialize, Serialize, Debug, Eq, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Text,
    Json,
}

struct PulsarSink {
    encoding: Encoding,
    producer: Arc<Mutex<TopicProducer<TokioExecutor>>>,
    pulsar: Pulsar<TokioExecutor>,
    in_flight: FuturesUnordered<MetadataFuture<SendFuture, usize>>,
    // ack
    seq_head: usize,
    seq_tail: usize,
    pending_acks: HashSet<usize>,
    acker: Acker,
}

type SendFuture =
    Box<dyn Future<Item = CommandSendReceipt, Error = pulsar::Error> + 'static + Send>;

inventory::submit! {
    SinkDescription::new_without_default::<PulsarSinkConfig>("pulsar")
}

#[typetag::serde(name = "pulsar")]
impl SinkConfig for PulsarSinkConfig {
    fn build(&self, cx: SinkContext) -> crate::Result<(super::RouterSink, super::Healthcheck)> {
        let sink = PulsarSink::new(self.clone(), cx.acker(), cx.exec())?;
        let hc = healthcheck(sink.producer.clone());
        Ok((Box::new(sink), hc))
    }

    fn input_type(&self) -> DataType {
        DataType::Log
    }

    fn sink_type(&self) -> &'static str {
        "pulsar"
    }
}

async fn create_producer(address: String, auth: Option<Authentication>, topic: String) -> Result<(Pulsar<TokioExecutor>, TopicProducer<TokioExecutor>), PulsarError> {
    let pulsar = Pulsar::new(&address, auth).await?;
    let producer = pulsar.create_producer(topic, None, ProducerOptions::default()).await?;
    Ok((pulsar, producer))
}

impl PulsarSink {
    pub(crate) fn new(
        config: PulsarSinkConfig,
        acker: Acker,
        exec: TaskExecutor,
    ) -> crate::Result<Self> {
        let auth = config.auth.map(|auth| Authentication {
            name: auth.name,
            data: auth.token.as_bytes().to_vec(),
        });
        let address = config.address.clone();
        let topic = config.topic.clone();
        let (sender, receiver) = channel();
        exec.spawn_std(async move {
            let res = create_producer(address, auth, topic).await;
            sender.send(res).unwrap();
        });
        let (pulsar, producer) = receiver.recv().unwrap().context(CreatePulsarSink)?;

        Ok(Self {
            encoding: config.encoding,
            pulsar,
            producer: Arc::new(Mutex::new(producer)),
            in_flight: FuturesUnordered::new(),
            seq_head: 0,
            seq_tail: 0,
            pending_acks: HashSet::new(),
            acker,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn pulsar(&self) -> &'_ Pulsar<TokioExecutor> {
        &self.pulsar
    }
}

impl Sink for PulsarSink {
    type SinkItem = Event;
    type SinkError = ();

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        let message = encode_event(item, self.encoding).map_err(|_| ())?;
        let producer = self.producer.clone();
        let fut = async move {
            producer.lock().await.send(message).await
        };

        let seqno = self.seq_head;
        self.seq_head += 1;
        self.in_flight
            .push((Box::new(Compat::new(Box::pin(fut))) as SendFuture).join(future::ok(seqno)));
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        loop {
            match self.in_flight.poll() {
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Ok(Async::Ready(None)) => return Ok(Async::Ready(())),
                Ok(Async::Ready(Some((result, seqno)))) => {
                    trace!(
                        "Pulsar sink produced message {:?} from {} at sequence id {}",
                        result.message_id,
                        result.producer_id,
                        result.sequence_id
                    );
                    self.pending_acks.insert(seqno);
                    let mut num_to_ack = 0;
                    while self.pending_acks.remove(&self.seq_tail) {
                        num_to_ack += 1;
                        self.seq_tail += 1;
                    }
                    self.acker.ack(num_to_ack);
                }
                Err(e) => error!("Pulsar sink generated an error: {}", e),
            }
        }
    }
}

fn encode_event(item: Event, enc: Encoding) -> crate::Result<Vec<u8>> {
    let log = item.into_log();
    let data = match enc {
        Encoding::Json => serde_json::to_vec(&log)?,
        Encoding::Text => log
            .get(&event::log_schema().message_key())
            .map(|v| v.as_bytes().to_vec())
            .unwrap_or_default(),
    };
    Ok(data)
}

fn healthcheck(producer: Arc<Mutex<TopicProducer<TokioExecutor>>>) -> super::Healthcheck {
    Box::new(Compat::new(Box::pin(async move {
        producer
            .lock()
            .await
            .check_connection()
            .await?;
        Ok(())
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{self, Event};
    use std::collections::HashMap;

    #[test]
    fn pulsar_event_json() {
        let msg = "hello_world".to_owned();
        let mut evt = Event::from(msg.clone());
        evt.as_mut_log().insert("key", "value");
        let result = encode_event(evt, Encoding::Json).unwrap();
        let map: HashMap<String, String> = serde_json::from_slice(&result[..]).unwrap();
        assert_eq!(msg, map[&event::log_schema().message_key().to_string()]);
    }

    #[test]
    fn pulsar_event_text() {
        let msg = "hello_world".to_owned();
        let evt = Event::from(msg.clone());
        let event = encode_event(evt, Encoding::Text).unwrap();

        assert_eq!(&event[..], msg.as_bytes());
    }
}

#[cfg(feature = "pulsar-integration-tests")]
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::test_util::{block_on, random_lines_with_stream, random_string};
    use pulsar::SubType;
    use futures::{compat::Compat, stream::StreamExt};
    use std::{
        sync::atomic::AtomicUsize,
        sync::{Arc, Mutex},
    };

    #[test]
    fn pulsar_happy() {
        let topic = format!("test-{}", random_string(10));
        let cnf = PulsarSinkConfig {
            address: "127.0.0.1:6650".to_owned(),
            topic: topic.clone(),
            encoding: Encoding::Text,
            auth: None,
        };
        let (acker, ack_counter) = Acker::new_for_testing();
        let rt = crate::runtime::Runtime::single_threaded().unwrap();

        let sink = PulsarSink::new(cnf, acker, rt.executor()).unwrap();

        let num_events = 1_000;
        let (_input, events) = random_lines_with_stream(100, num_events);
        let pulsar = sink.pulsar().clone();
        let mut consumer = block_on(Compat::new(Box::pin(async move {
            pulsar
                .consumer()
                .with_topic(&topic)
                .with_consumer_name("VectorTestConsumer")
                .with_subscription_type(SubType::Shared)
                .with_subscription("VectorTestSub")
                .build::<String>()
                .await
        }))).unwrap();

        let pump = sink.send_all(events);
        let _ = block_on(pump).unwrap();

        assert_eq!(
            ack_counter.load(std::sync::atomic::Ordering::Relaxed),
            num_events
        );

        let error: Arc<Mutex<Option<pulsar::Error>>> = Arc::new(Mutex::new(None));
        let successes = Arc::new(AtomicUsize::new(0));

        {
            let successes = successes.clone();
            block_on(Compat::new(Box::pin(async move {
                for _ in 0..1000u16 {
                    let msg = match consumer.next().await.unwrap() {
                        Ok(msg) => msg,
                        Err(err) => {
                            *error.lock().unwrap() = Some(err);
                            break;
                        }
                    };
                    consumer.ack(&msg).unwrap();
                    successes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                crate::Result::Ok(())
            }))).unwrap();
        }
        assert_eq!(successes.load(std::sync::atomic::Ordering::Relaxed), 1000);
    }
}
