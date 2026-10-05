//! The QUIC carrier: noq-proto with time, datagrams, and randomness as inputs.

mod settings;

#[cfg(test)]
mod testing {
    use std::num::{NonZeroU32, NonZeroUsize};
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use block::{Heap, Pool};
    use env::clock::Clock;
    use env::entropy::Entropy;
    use env::tasks::Tasks;
    use types::node::PrivateKey;
    use types::time::Span;

    use crate::Config;

    /// What one sim shard gives a [`Config`].
    pub(super) struct Shard {
        pub(super) clock: Clock,
        entropy: Entropy,
        tasks: Tasks,
        pool: Rc<Pool>,
    }

    impl Shard {
        /// A config for a node with `private_key`, on this shard.
        pub(super) fn config(&self, private_key: PrivateKey) -> Config {
            Config {
                private_key,
                message_bytes_max: NonZeroUsize::new(1 << 16).expect("not zero"),
                window_bytes: 1 << 20,
                streams_max: NonZeroU32::new(16).expect("not zero"),
                idle: Span::SECOND,
                clock: self.clock.clone(),
                entropy: self.entropy.clone(),
                tasks: self.tasks.clone(),
                pool: Rc::clone(&self.pool),
            }
        }
    }

    /// Runs `test` on one shard of a sim run made from `value`, and gives its result.
    pub(super) fn run<T: Send + 'static>(
        value: u64,
        test: impl FnOnce(&Shard) -> T + Send + 'static,
    ) -> T {
        let mut sim = sim::Sim::new(sim::Config {
            seed: value,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config::default());
        let (clock, entropy) = (node.clock(), node.entropy());
        let shard = env::shards::Config {
            name: "shard-0".into(),
            core: Some(0),
        };
        let result = Arc::new(Mutex::new(None));
        let out = Arc::clone(&result);
        let handle = node.shards().start(shard, move |tasks| async move {
            let config = block::Config { budget: 1 << 22 };
            let memory = Heap::new(config.reservation());
            let shard = Shard {
                clock,
                entropy,
                tasks,
                pool: Rc::new(Pool::new(config, memory)),
            };
            *out.lock().expect("not poisoned") = Some(test(&shard));
        });
        sim.run().expect("the run ends");
        handle
            .expect("the shard starts")
            .join()
            .expect("the test passes");
        let result = result.lock().expect("not poisoned").take();
        result.expect("the test ran")
    }
}
