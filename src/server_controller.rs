/*!
 * Facilities for interacting with Server Controllers.  See RFD 48.
 */

use crate::api_error::ApiError;
use crate::api_model::ApiInstance;
use crate::api_model::ApiInstanceRuntimeState;
use crate::api_model::ApiInstanceRuntimeStateParams;
use crate::api_model::ApiInstanceState;
use crate::controller::ControllerScApi;
use async_trait::async_trait;
use chrono::Utc;
use futures::channel::mpsc::Receiver;
use futures::channel::mpsc::Sender;
use futures::lock::Mutex;
use futures::stream::StreamExt;
use slog::Logger;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/**
 * `ServerController` is a handle for the software service running on a compute
 * server that manages the control plane on that server.  The current
 * implementation simulates a server directly in this program.
 *
 * **It's important to be careful about the interface exposed by this struct.**
 * The intent is for it to eventually be implemented using requests to a remote
 * server.  The tighter the coupling that exists now, the harder this will be to
 * move later.
 */
pub struct ServerController {
    /** unique id for this server */
    pub id: Uuid,

    /** indicates how to simulate instance transitions */
    pub sim_mode: ServerControllerSimMode,

    /** handle for the internal control plane API */
    ctlsc: ControllerScApi,
    /** debug log */
    log: Logger,
    /** collection of simulated instances, indexed by instance uuid */
    instances: Mutex<BTreeMap<Uuid, SimInstance>>,
}

#[derive(Copy, Clone)]
pub enum ServerControllerSimMode {
    Auto,
    Api,
}

impl ServerController {
    /** Constructs a simulated ServerController with the given uuid. */
    pub fn new_simulated_with_id(
        id: &Uuid,
        sim_mode: ServerControllerSimMode,
        log: Logger,
        ctlsc: ControllerScApi,
    ) -> ServerController {
        info!(log, "created server controller");

        ServerController {
            id: id.clone(),
            sim_mode,
            log,
            ctlsc,
            instances: Mutex::new(BTreeMap::new()),
        }
    }

    /**
     * Idempotently ensures that the given API Instance (described by
     * `api_instance`) exists on this server in the given runtime state
     * (described by `target`).
     */
    pub async fn instance_ensure(
        self: &Arc<Self>,
        api_instance: Arc<ApiInstance>,
        target: &ApiInstanceRuntimeStateParams,
    ) -> Result<ApiInstanceRuntimeState, ApiError> {
        if target.reboot_wanted && target.run_state != ApiInstanceState::Running
        {
            return Err(ApiError::InvalidRequest {
                message: String::from(
                    "cannot reboot to a state other than \"running\"",
                ),
            });
        }

        let id = api_instance.identity.id.clone();
        let mut instances = self.instances.lock().await;
        let maybe_current_instance = instances.remove(&id);

        let (mut instance, is_new) = {
            if let Some(current_instance) = maybe_current_instance {
                (current_instance, false)
            } else {
                /* Create a new Instance. */
                let idc = id.clone();
                let log = self.log.new(o!("instance_id" => idc.to_string()));

                if let ServerControllerSimMode::Auto = self.sim_mode {
                    let (instance, rx) = SimInstance::new_simulated_auto(
                        &api_instance.runtime,
                        log,
                    );
                    let selfc = Arc::clone(&self);
                    tokio::spawn(async move {
                        selfc.instance_sim(idc, rx).await;
                    });
                    (instance, true)
                } else {
                    (
                        SimInstance::new_simulated_explicit(
                            &api_instance.runtime,
                            log,
                        ),
                        true,
                    )
                }
            }
        };

        let current_state = &instance.current_run_state.run_state;
        let rv = if target.reboot_wanted
            && !is_new
            && *current_state != ApiInstanceState::Starting
            && *current_state != ApiInstanceState::Running
            && (*current_state != ApiInstanceState::Stopping
                || !instance.current_run_state.reboot_in_progress)
        {
            Err(ApiError::InvalidRequest {
                message: format!(
                    "cannot reboot instance in state \"{}\"",
                    current_state
                ),
            })
        } else {
            instance.transition(target);
            Ok(instance.current_run_state.clone())
        };

        instances.insert(id.clone(), instance);
        rv
    }

    /**
     * Body of the background task (one per `SimInstance`) that simulates
     * Instance booting and halting.  Each time we read a message from the
     * instance's channel, we sleep for a bit and then invoke `instance_poke()`
     * to complete whatever transition is currently outstanding.
     */
    async fn instance_sim(&self, id: Uuid, mut rx: Receiver<()>) {
        while let Some(_) = rx.next().await {
            tokio::time::delay_for(Duration::from_millis(1500)).await;
            self.instance_poke(id).await;
        }
    }

    /**
     * Invoked as part of simulation to complete whatever asynchronous
     * transition is currently going on for instance `id`.
     */
    async fn instance_poke(&self, id: Uuid) {
        let (new_state, to_destroy) = {
            /* Do as little as possible with the lock held. */
            let mut instances = self.instances.lock().await;
            let mut instance = instances.remove(&id).unwrap();
            instance.transition_finish();
            let after = instance.current_run_state.clone();
            if instance.requested_run_state.is_none()
                && after.run_state == ApiInstanceState::Destroyed
            {
                info!(instance.log, "instance came to rest destroyed");
                (after, Some(instance))
            } else {
                instances.insert(id.clone(), instance);
                (after, None)
            }
        };

        /*
         * Notify the controller that the instance state has changed.  The
         * server controller is authoritative for the runtime state, and we use
         * a generation number here so that calls processed out of order do not
         * settle on the wrong value.
         * TODO-robustness: If this fails, we need to put it on some list of
         * updates to retry later.
         */
        self.ctlsc.notify_instance_updated(&id, &new_state).await.unwrap();

        /*
         * If the instance came to rest destroyed, complete any async cleanup
         * needed now.
         * TODO-debug It would be nice to have visibility into instances that
         * are cleaning up in case we have to debug resource leaks here.
         * TODO-correctness Is it a problem that nobody waits on the background
         * task?  If we did it here, we'd deadlock, since we're invoked from the
         * background task.
         */
        if let Some(destroyed_instance) = to_destroy {
            if let Some(mut tx) = destroyed_instance.channel_tx {
                tx.close_channel();
            }
        }
    }
}

/**
 * Trait used to expose interfaces for use only by the test suite.
 */
#[async_trait]
pub trait ServerControllerTestInterfaces {
    async fn instance_finish_transition(&self, id: Uuid);
}

#[async_trait]
impl ServerControllerTestInterfaces for ServerController {
    async fn instance_finish_transition(&self, id: Uuid) {
        self.instance_poke(id).await
    }
}

/**
 * `SimInstance` simulates an Oxide Rack Instance (virtual machine), as created
 * by the public API.
 *
 * We only simulate the Instance from the perspective of an API consumer, which
 * means for example accepting a request to boot it, reporting the current state
 * as "starting", and then some time later reporting that the state is
 * "running".
 */
struct SimInstance {
    /** Current runtime state of the instance */
    current_run_state: ApiInstanceRuntimeState,
    /**
     * Requested runtime state of the instance.  This field is non-None if and
     * only if we're currently simulating an asynchronous transition (e.g., boot
     * or halt).
     */
    requested_run_state: Option<ApiInstanceRuntimeStateParams>,

    /** Debug log */
    log: Logger,
    /** Channel for transmitting to the background task */
    channel_tx: Option<Sender<()>>,
}

/**
 * Buffer size for channel used to communicate with each SimInstance's
 * background task.  Messages sent on this channel trigger the task to simulate
 * an Instance state transition by sleeping for some interval and then updating
 * the Instance state.  When the background task updates the Instance state
 * after sleeping, it always looks at the current state to decide what to do.
 * As a result, we never need to queue up more than one transition.  In turn,
 * that means we don't need (or want) a channel buffer larger than 1.  If we
 * were to queue up multiple messages in the buffer, the net effect would be
 * exactly the same as if just one message were queued.  (Because of what we
 * said above, as part of processing that message, the receiver will wind up
 * handling all state transitions requested up to the point where the first
 * message is read.  If another transition is requested after that point,
 * another message will be enqueued and the receiver will process that
 * transition then.  There's no need to queue more than one message.)  Even
 * stronger: we don't want a larger buffer because that would only cause extra
 * laps through the sleep cycle, which just wastes resources and increases the
 * latency for processing the next real transition request.
 */
const SIM_INSTANCE_CHANNEL_BUFFER_SIZE: usize = 0;

impl SimInstance {
    /**
     * Create a new `SimInstance` with state transitions automatically
     * simulated by a background task.  The caller is expected to provide the
     * background task that reads from the channel and advances the simulation.
     */
    fn new_simulated_auto(
        initial_runtime: &ApiInstanceRuntimeState,
        log: Logger,
    ) -> (SimInstance, Receiver<()>) {
        debug!(log, "created simulated instance";
            "initial_state" => ?initial_runtime);
        let (tx, rx) =
            futures::channel::mpsc::channel(SIM_INSTANCE_CHANNEL_BUFFER_SIZE);
        (
            SimInstance {
                current_run_state: initial_runtime.clone(),
                requested_run_state: None,
                log,
                channel_tx: Some(tx),
            },
            rx,
        )
    }

    /**
     * Create a new `SimInstance` with state transitions simulated by explicit
     * calls.  The only difference from the perspective of this struct is that
     * we won't have a channel to which we send notifications when asynchronous
     * state transitions begin.
     */
    fn new_simulated_explicit(
        initial_runtime: &ApiInstanceRuntimeState,
        log: Logger,
    ) -> SimInstance {
        debug!(log, "created simulated instance";
            "initial_state" => ?initial_runtime);
        SimInstance {
            current_run_state: initial_runtime.clone(),
            requested_run_state: None,
            log,
            channel_tx: None,
        }
    }

    /**
     * Transition this Instance to state `given_target`.  In some cases, the
     * transition may happen immediately (e.g., going from "Stopped" to
     * "Destroyed").  In other cases, as when going from "Stopped" to "Running",
     * we immediately transition to an intermediate state ("Starting", in this
     * case), simulate the transition, and some time later update to the desired
     * state.
     *
     * This function supports transitions that don't change the state at all
     * (either because the requested state change was `None` -- maybe we were
     * changing some other runtime state parameter -- or because we're already
     * in the desired state).
     */
    fn transition(
        &mut self,
        given_target: &ApiInstanceRuntimeStateParams,
    ) -> Option<ApiInstanceRuntimeStateParams> {
        /*
         * In all cases, set `requested_run_state` to the new target.  If there
         * was already a requested run state, we will return this to the caller
         * so that they can log a possible dropped transition.  This is only
         * intended for debugging.
         */
        let dropped = self.requested_run_state.take();
        let state_before = self.current_run_state.run_state.clone();
        let mut state_after = match given_target.run_state {
            /*
             * For intermediate states (which don't really make sense to
             * request), just try to do the closest reasonable thing.
             * TODO-cleanup Use a different type here.
             */
            ApiInstanceState::Creating => &ApiInstanceState::Running,
            ApiInstanceState::Starting => &ApiInstanceState::Running,
            ApiInstanceState::Stopping => &ApiInstanceState::Stopped,

            /* This is the most common interesting case. */
            ref target_run_state => target_run_state,
        };

        /*
         * There's nothing to do if the current and target states are the same
         * AND either:
         *
         * - there's neither a reboot pending nor a reboot requested
         * - there's both a reboot pending and a reboot requested and
         *   the current reboot is still in the "Stopping" phase
         *
         * Otherwise, even if the states match, we may need to take action to
         * begin or cancel a reboot.
         *
         * Reboot can only be requested with a target state of "Running".
         * It doesn't make sense to reboot to any other state.
         * TODO-debug log a warning in this case or make it impossible to
         * represent?  Or validate it sooner?  TODO-cleanup if we create a
         * separate ApiInstanceStateRequested as discussed elsewhere, the
         * `Running` state could have a boolean indicating whether a reboot
         * is requested first.
         */
        let reb_pending = self.current_run_state.reboot_in_progress;
        let reb_wanted = *state_after == ApiInstanceState::Running
            && given_target.reboot_wanted;
        if *state_after == state_before
            && ((!reb_pending && !reb_wanted)
                || (reb_pending
                    && reb_wanted
                    && state_before == ApiInstanceState::Stopping))
        {
            debug!(self.log, "noop transition"; "target" => ?given_target);
            return dropped;
        }

        /*
         * If we're doing a reboot, then we've already verified that the target
         * run state is "Running", but for the rest of this function we'll treat
         * it like a transition to "Stopped" (with an extra bit telling us later
         * to transition again to Running).
         */
        if reb_wanted {
            state_after = &ApiInstanceState::Stopped;
        }

        /*
         * Depending on what state we're in and what state we're going to, we
         * may need to transition to an intermediate state before we can get to
         * the requested state.  In that case, we'll asynchronously simulate the
         * transition.
         */
        let (immed_next_state, need_async) =
            if state_before.is_stopped() && !state_after.is_stopped() {
                (&ApiInstanceState::Starting, true)
            } else if !state_before.is_stopped() && state_after.is_stopped() {
                (&ApiInstanceState::Stopping, true)
            } else {
                (state_after, false)
            };

        /*
         * Update the current state to reflect what we've decided -- either
         * going directly to the requested state or to an intermediate state.
         */
        self.current_run_state = ApiInstanceRuntimeState {
            run_state: immed_next_state.clone(),
            reboot_in_progress: reb_wanted,
            server_uuid: self.current_run_state.server_uuid.clone(),
            gen: self.current_run_state.gen + 1,
            time_updated: Utc::now(),
        };

        debug!(self.log, "instance transition";
            "state_before" => %state_before,
            "state_after" => %state_after,
            "immed_next_state" => %immed_next_state,
            "dropped" => ?dropped,
            "async" => %need_async,
            "new_runtime" => ?self.current_run_state
        );

        /*
         * If this is an asynchronous transition, notify the background task to
         * simulate it.  There are a few possible error cases:
         *
         * (1) We fail to send the message because the channel's buffer is full.
         *     All we need to guarantee in the first place is that the receiver
         *     will receive a message at least once after this function is
         *     invoked.  If there's already a message in the buffer, we don't
         *     need to do anything else to achieve that.
         *
         * (2) We fail to send the message because the channel is disconnected.
         *     This would be a programmer error -- the contract between us and
         *     the receiver is that we shut down the channel first.  As a
         *     result, we panic if we find this case.
         *
         * (3) We failed to send the message for some other reason.  This
         *     appears impossible at the time of this writing.   It would be
         *     nice if the returned error type were implemented in a way that we
         *     could identify this case at compile time (e.g., using an enum),
         *     but that's not currently the case.
         */
        if need_async {
            self.requested_run_state = Some(ApiInstanceRuntimeStateParams {
                run_state: state_after.clone(),
                reboot_wanted: reb_wanted,
            });
            if let Some(ref mut tx) = self.channel_tx {
                let result = tx.try_send(());
                if let Err(error) = result {
                    assert!(!error.is_disconnected());
                    assert!(error.is_full());
                }
            }
        }

        dropped
    }

    /**
     * Finish simulating a "boot" or "halt" transition.
     */
    fn transition_finish(&mut self) {
        let requested_run_state = match self.requested_run_state.take() {
            /*
             * Somebody must have requested a state change while we were
             * simulating a previous asynchronous one.  By definition, the new
             * one must also be asynchronous, and the first of the two calls to
             * `transition_finish()` will complete the new transition.  The
             * second one will find us here.
             * TODO-cleanup We could probably eliminate this case by not
             * sending a message to the background task if we were already in an
             * async transition.
             */
            None => {
                debug!(self.log, "noop transition finish";
                    "current_run_state" => %self.current_run_state.run_state);
                return;
            }
            Some(run_state) => run_state,
        };

        /*
         * As documented above, `self.requested_run_state` is only non-None when
         * there's an asynchronous (simulated) transition in progress, and the
         * only such transitions start at "Starting" or "Stopping" and go to
         * "Running" or one of several stopped states, respectively.  Since we
         * checked `self.requested_run_state` above, we know we're in one of
         * these two transitions and assert that here.
         */
        let run_state_before = self.current_run_state.run_state.clone();
        let run_state_after = requested_run_state.run_state;
        match run_state_before {
            ApiInstanceState::Starting => {
                assert_eq!(run_state_after, ApiInstanceState::Running);
                assert!(!requested_run_state.reboot_wanted);
            }
            ApiInstanceState::Stopping => {
                assert!(run_state_after.is_stopped());
                assert_eq!(
                    requested_run_state.reboot_wanted,
                    self.current_run_state.reboot_in_progress
                );
                assert!(
                    !requested_run_state.reboot_wanted
                        || run_state_after == ApiInstanceState::Stopped
                );
            }
            _ => panic!("async transition started for unexpected state"),
        };

        /*
         * Having verified all that, we can update the Instance's state.
         */
        self.current_run_state = ApiInstanceRuntimeState {
            run_state: run_state_after.clone(),
            reboot_in_progress: requested_run_state.reboot_wanted,
            server_uuid: self.current_run_state.server_uuid.clone(),
            gen: self.current_run_state.gen + 1,
            time_updated: Utc::now(),
        };

        debug!(self.log, "simulated transition finish";
            "state_before" => %run_state_before,
            "state_after" => %run_state_after,
            "new_runtime" => ?self.current_run_state
        );

        if self.current_run_state.reboot_in_progress {
            assert_eq!(run_state_after, ApiInstanceState::Stopped);
            self.transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Running,
                reboot_wanted: false,
            });
        }
    }
}

#[cfg(test)]
mod test {
    use super::SimInstance;
    use crate::api_model::ApiInstanceRuntimeState;
    use crate::api_model::ApiInstanceRuntimeStateParams;
    use crate::api_model::ApiInstanceState;
    use crate::test_util::test_setup_log;
    use chrono::Utc;
    use dropshot::test_util::LogContext;
    use futures::channel::mpsc::Receiver;

    fn make_instance(
        logctx: &LogContext,
        initial_state: ApiInstanceState,
    ) -> (SimInstance, Receiver<()>) {
        let now = Utc::now();
        let initial_runtime = {
            ApiInstanceRuntimeState {
                run_state: initial_state,
                reboot_in_progress: false,
                server_uuid: uuid::Uuid::new_v4(),
                gen: 1,
                time_updated: now,
            }
        };

        SimInstance::new_simulated_auto(&initial_runtime, logctx.log.new(o!()))
    }

    /*
     * Test non-reboot-related transitions.
     */
    #[tokio::test]
    async fn test_sim_instance() {
        let logctx = test_setup_log("test_sim_instance").await;
        let (mut instance, mut rx) =
            make_instance(&logctx, ApiInstanceState::Creating);
        let r1 = instance.current_run_state.clone();

        info!(logctx.log, "new instance"; "run_state" => ?r1.run_state);
        assert_eq!(r1.run_state, ApiInstanceState::Creating);
        assert_eq!(r1.gen, 1);

        /*
         * There's no asynchronous transition going on yet so a
         * transition_finish() shouldn't change anything.
         */
        assert!(instance.requested_run_state.is_none());
        instance.transition_finish();
        assert!(instance.requested_run_state.is_none());
        assert_eq!(&r1.time_updated, &instance.current_run_state.time_updated);
        assert_eq!(&r1.run_state, &instance.current_run_state.run_state);
        assert_eq!(r1.gen, instance.current_run_state.gen);
        assert!(rx.try_next().is_err());

        /*
         * We should be able to transition immediately to any other stopped
         * state.  We can't do this for "Creating" because transition() treats
         * that as a transition to "Running".
         */
        let stopped_states = vec![
            ApiInstanceState::Stopped,
            ApiInstanceState::Repairing,
            ApiInstanceState::Failed,
            ApiInstanceState::Destroyed,
        ];
        let mut rprev = r1;
        for state in stopped_states {
            assert!(rprev.run_state.is_stopped());
            let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
                run_state: state.clone(),
                reboot_wanted: false,
            });
            assert!(dropped.is_none());
            assert!(instance.requested_run_state.is_none());
            let rnext = instance.current_run_state.clone();
            if state != rprev.run_state {
                assert!(rnext.gen > rprev.gen);
            }
            assert!(rnext.time_updated >= rprev.time_updated);
            assert_eq!(rnext.run_state, state);
            assert!(rx.try_next().is_err());
            rprev = rnext;
        }

        /*
         * Now, if we transition to "Running", we must go through the async
         * process.
         */
        assert!(rprev.run_state.is_stopped());
        assert!(rx.try_next().is_err());
        let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
            run_state: ApiInstanceState::Running,
            reboot_wanted: false,
        });
        assert!(dropped.is_none());
        assert!(instance.requested_run_state.is_some());
        assert!(rx.try_next().is_ok());
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert_eq!(rnext.run_state, ApiInstanceState::Starting);
        assert!(!rnext.run_state.is_stopped());
        rprev = rnext;

        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert!(instance.requested_run_state.is_none());
        assert!(rx.try_next().is_err());
        assert_eq!(rprev.run_state, ApiInstanceState::Starting);
        assert_eq!(rnext.run_state, ApiInstanceState::Running);
        rprev = rnext;
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rprev.gen, rnext.gen);

        /*
         * If we transition again to "Running", the process should complete
         * immediately.
         */
        assert!(!rprev.run_state.is_stopped());
        let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
            run_state: ApiInstanceState::Running,
            reboot_wanted: false,
        });
        assert!(dropped.is_none());
        assert!(instance.requested_run_state.is_none());
        assert!(rx.try_next().is_err());
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.gen, rprev.gen);
        assert_eq!(rnext.time_updated, rprev.time_updated);
        assert_eq!(rnext.run_state, rprev.run_state);
        rprev = rnext;

        /*
         * If we go back to any stopped state, we go through the async process
         * again.
         */
        assert!(!rprev.run_state.is_stopped());
        assert!(rx.try_next().is_err());
        let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
            run_state: ApiInstanceState::Destroyed,
            reboot_wanted: false,
        });
        assert!(dropped.is_none());
        assert!(instance.requested_run_state.is_some());
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert_eq!(rnext.run_state, ApiInstanceState::Stopping);
        assert!(!rnext.run_state.is_stopped());
        rprev = rnext;

        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert!(instance.requested_run_state.is_none());
        assert_eq!(rprev.run_state, ApiInstanceState::Stopping);
        assert_eq!(rnext.run_state, ApiInstanceState::Destroyed);
        rprev = rnext;
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rprev.gen, rnext.gen);

        /*
         * Now let's test the behavior of dropping a transition.  We'll start
         * transitioning back to "Running".  Then, while we're still in
         * "Starting", will transition back to "Destroyed".  We should
         * immediately go to "Stopping", and completing the transition should
         * take us to "Destroyed".
         */
        assert!(rprev.run_state.is_stopped());
        let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
            run_state: ApiInstanceState::Running,
            reboot_wanted: false,
        });
        assert!(dropped.is_none());
        assert!(instance.requested_run_state.is_some());
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert_eq!(rnext.run_state, ApiInstanceState::Starting);
        assert!(!rnext.run_state.is_stopped());
        rprev = rnext;

        /*
         * Interrupt the async transition with a new one.
         */
        let dropped = instance.transition(&ApiInstanceRuntimeStateParams {
            run_state: ApiInstanceState::Destroyed,
            reboot_wanted: false,
        });
        assert_eq!(dropped.unwrap().run_state, ApiInstanceState::Running);
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert_eq!(rnext.run_state, ApiInstanceState::Stopping);
        rprev = rnext;

        /*
         * Finish the async transition.
         */
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated >= rprev.time_updated);
        assert!(instance.requested_run_state.is_none());
        assert_eq!(rprev.run_state, ApiInstanceState::Stopping);
        assert_eq!(rnext.run_state, ApiInstanceState::Destroyed);
        rprev = rnext;
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rprev.gen, rnext.gen);

        logctx.cleanup_successful();
    }

    /*
     * Test reboot-related transitions.
     */
    #[tokio::test]
    async fn test_sim_instance_reboot() {
        let logctx = test_setup_log("test_sim_instance_reboot").await;

        /*
         * Get an initial instance up to "Running".
         */
        let (mut instance, _rx) =
            make_instance(&logctx, ApiInstanceState::Creating);
        let r1 = instance.current_run_state.clone();

        info!(logctx.log, "new instance"; "run_state" => ?r1.run_state);
        assert_eq!(r1.run_state, ApiInstanceState::Creating);
        assert_eq!(r1.gen, 1);
        assert!(instance
            .transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Running,
                reboot_wanted: false,
            })
            .is_none());
        instance.transition_finish();
        let (rprev, rnext) = (r1, instance.current_run_state.clone());
        assert!(rnext.gen > rprev.gen);

        /*
         * Now, take it through a reboot sequence.
         */
        assert!(instance
            .transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Running,
                reboot_wanted: true,
            })
            .is_none());
        let (rprev, rnext) = (rnext, instance.current_run_state.clone());
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated > rprev.time_updated);
        assert_eq!(rnext.run_state, ApiInstanceState::Stopping);
        assert!(rnext.reboot_in_progress);
        assert!(instance.requested_run_state.is_some());
        instance.transition_finish();
        let (rprev, rnext) = (rnext, instance.current_run_state.clone());
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated > rprev.time_updated);
        assert_eq!(rnext.run_state, ApiInstanceState::Starting);
        assert!(!rnext.reboot_in_progress);
        assert!(instance.requested_run_state.is_some());
        instance.transition_finish();
        let (rprev, rnext) = (rnext, instance.current_run_state.clone());
        assert!(rnext.gen > rprev.gen);
        assert!(rnext.time_updated > rprev.time_updated);
        assert_eq!(rnext.run_state, ApiInstanceState::Running);
        assert!(instance.requested_run_state.is_none());

        /*
         * Begin a reboot.  Then, while it's still "Stopping", begin another
         * reboot.  This should go through exactly one reboot sequence, as the
         * second reboot is totally superfluous.
         */
        assert!(instance
            .transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Running,
                reboot_wanted: true,
            })
            .is_none());
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Stopping);
        assert!(instance
            .transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Running,
                reboot_wanted: true,
            })
            .is_some());
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Stopping);
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Starting);
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Running);
        assert!(instance.requested_run_state.is_none());
        instance.transition_finish();
        let (rprev, rnext) = (rnext, instance.current_run_state.clone());
        assert_eq!(rprev.gen, rnext.gen);

        /*
         * Begin a reboot.  Then, while it's "Starting" (on the way back up),
         * begin another reboot.  This should go through a second reboot
         * sequence.
         */
        assert!(instance
            .transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Running,
                reboot_wanted: true,
            })
            .is_none());
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Stopping);
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Starting);
        assert!(instance
            .transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Running,
                reboot_wanted: true,
            })
            .is_some());
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Stopping);
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Starting);
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Running);
        assert!(instance.requested_run_state.is_none());
        instance.transition_finish();
        let (rprev, rnext) = (rnext, instance.current_run_state.clone());
        assert_eq!(rprev.gen, rnext.gen);

        /*
         * At this point, we've exercised what happens when a reboot is issued
         * from "Running", from "Starting" with a reboot in progress, from
         * "Stopping" with a reboot in progress.  All that's left is "Starting"
         * with no reboot in progress.  First, stop the instance.  Then start
         * it.  Then, while it's starting, begin a reboot sequence.
         */
        assert!(instance
            .transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Stopped,
                reboot_wanted: false,
            })
            .is_none());
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Stopped);
        assert!(instance
            .transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Running,
                reboot_wanted: false,
            })
            .is_none());
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Starting);
        assert!(instance
            .transition(&ApiInstanceRuntimeStateParams {
                run_state: ApiInstanceState::Running,
                reboot_wanted: true,
            })
            .is_some());
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Stopping);
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Starting);
        instance.transition_finish();
        let rnext = instance.current_run_state.clone();
        assert_eq!(rnext.run_state, ApiInstanceState::Running);
        assert!(instance.requested_run_state.is_none());
        instance.transition_finish();
        let (rprev, rnext) = (rnext, instance.current_run_state.clone());
        assert_eq!(rprev.gen, rnext.gen);

        /*
         * Issuing a reboot from any other state is not defined, including from
         * "Stopping" while not in the process of a reboot and from any
         * "stopped" state.  instance_ensure() will prevent this, while
         * transition() will allow it.  We don't test the behavior of
         * transition() because it's subject to change.
         */

        logctx.cleanup_successful();
    }
}