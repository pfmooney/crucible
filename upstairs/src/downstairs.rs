use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
};

use crate::{
    cdt,
    client::{ClientAction, ClientStopReason, DownstairsClient},
    live_repair::ExtentInfo,
    stats::UpStatOuter,
    upstairs::{UpstairsConfig, UpstairsState},
    AckStatus, ActiveJobs, AllocRingBuffer, BlockOp, BlockReq, BlockReqWaiter,
    ClientData, ClientIOStateCount, ClientId, ClientMap, CrucibleError,
    DownstairsIO, DownstairsMend, DsState, ExtentFix, ExtentRepairIDs, GtoS,
    GuestWork, IOState, IOStateCount, IOop, ImpactedBlocks, JobId, Message,
    ReadRequest, ReadResponse, ReconcileIO, ReconciliationId, RegionDefinition,
    ReplaceResult, SnapshotDetails, WorkSummary,
};
use crucible_common::MAX_ACTIVE_COUNT;

use rand::prelude::*;
use ringbuffer::RingBuffer;
use slog::{debug, error, info, o, warn, Logger};
use tokio::sync::oneshot;
use uuid::Uuid;

/*
 * The structure that tracks information about the three downstairs
 * connections as well as the work that each is doing.
 */
#[derive(Debug)]
pub(crate) struct Downstairs {
    /// Shared configuration
    cfg: Arc<UpstairsConfig>,

    /// Per-client data
    pub(crate) clients: ClientData<DownstairsClient>,

    /// The active list of IO for the downstairs.
    ds_active: ActiveJobs,

    /// The number of write bytes that haven't finished yet
    ///
    /// This is used to configure backpressure to the host, because writes
    /// (uniquely) will return before actually being completed by a Downstairs
    /// and can clog up the queues.
    ///
    /// It is stored in the Downstairs because from the perspective of the
    /// Upstairs, writes complete immediately; only the Downstairs is actually
    /// tracking the pending jobs.
    write_bytes_outstanding: u64,

    /// The next Job ID this Upstairs should use for downstairs work.
    next_id: JobId,

    /// Current flush number
    ///
    /// This is the highest flush number from all three downstairs on startup,
    /// and increments by one each time the guest sends a flush (including
    /// automatic flushes).
    next_flush: u64,

    /// Ringbuf of completed downstairs job IDs.
    completed: AllocRingBuffer<JobId>,

    /// Ringbuf of a summary of each recently completed downstairs IO.
    completed_jobs: AllocRingBuffer<WorkSummary>,

    /// Current piece of reconcile work that the downstairs are working on
    ///
    /// It can be New, InProgress, Skipped, or Done.
    reconcile_current_work: Option<ReconcileIO>,

    /// Remaining reconciliation work
    ///
    /// This queue holds the remaining work required to make all three
    /// downstairs in a region set the same.
    reconcile_task_list: VecDeque<ReconcileIO>,

    /// Number of extents repaired during initial activation
    reconcile_repaired: usize,

    /// Number of extents needing repair during initial activation
    reconcile_repair_needed: usize,

    /// The logger for messages sent from downstairs methods.
    log: Logger,

    /// Data for an in-progress live repair
    repair: Option<LiveRepairData>,

    /// Jobs that are ready to be acked
    ///
    /// This must be handled after every event
    ackable_work: BTreeSet<JobId>,
}

/// State machine for a live-repair operation
///
/// We pass through all states (except `FinalFlush`) in order for each extent,
/// then pass through the `FinalFlush` state before completion.  In each state,
/// we're waiting for a particular job to finish, which is indicated by a
/// `BlockReqWaiter`.
///
/// Early states carry around reserved IDs (both `JobId` and guest work IDs), as
/// well as a reserved `BlockReqWaiter` for the final flush.
enum LiveRepairState {
    // TODO remove the BlockReqWaiters here, since we can handle the
    // `ExtentLive*Ack*` messages as they come in?
    //
    // Right now, they're handled in `process_io_completion`
    Closing {
        close_id: JobId,
        repair_id: JobId,
        noop_id: JobId,
        reopen_id: JobId,

        gw_repair_id: u64,
        gw_noop_id: u64,

        close_brw: BlockReqWaiter,
        reopen_brw: BlockReqWaiter,
    },
    Repairing {
        repair_id: JobId,
        noop_id: JobId,
        reopen_id: JobId,

        gw_noop_id: u64,

        repair_brw: BlockReqWaiter,
        reopen_brw: BlockReqWaiter,
    },
    Noop {
        noop_id: JobId,
        reopen_id: JobId,

        noop_brw: BlockReqWaiter,
        reopen_brw: BlockReqWaiter,
    },
    Reopening {
        reopen_id: JobId,
        reopen_brw: BlockReqWaiter,
    },
    FinalFlush {
        flush_id: JobId,
        flush_brw: BlockReqWaiter,
    },

    /// Placeholder value when we're in the process of modifying the state
    ///
    /// This is needed because `BlockReqWaiter` isn't `Clone`.
    Swapping,
}

// For this `Debug` implementation, we skip the `BlockReqWaiter`s
impl std::fmt::Debug for LiveRepairState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiveRepairState::Closing {
                close_id,
                repair_id,
                noop_id,
                reopen_id,
                gw_repair_id,
                gw_noop_id,
                ..
            } => f
                .debug_struct("LiveRepairState::Closing")
                .field("close_id", close_id)
                .field("repair_id", repair_id)
                .field("noop_id", noop_id)
                .field("reopen_id", reopen_id)
                .field("gw_repair_id", gw_repair_id)
                .field("gw_noop_id", gw_noop_id)
                .finish(),
            LiveRepairState::Repairing {
                repair_id,
                noop_id,
                reopen_id,
                gw_noop_id,
                ..
            } => f
                .debug_struct("LiveRepairState::Repairing")
                .field("repair_id", repair_id)
                .field("noop_id", noop_id)
                .field("reopen_id", reopen_id)
                .field("gw_noop_id", gw_noop_id)
                .finish(),
            LiveRepairState::Noop {
                noop_id, reopen_id, ..
            } => f
                .debug_struct("LiveRepairState::Noop")
                .field("noop_id", noop_id)
                .field("reopen_id", reopen_id)
                .finish(),
            LiveRepairState::Reopening { reopen_id, .. } => f
                .debug_struct("LiveRepairState::Reopening")
                .field("reopen_id", reopen_id)
                .finish(),
            LiveRepairState::FinalFlush { flush_id, .. } => f
                .debug_struct("LiveRepairState::FinalFlush")
                .field("flush_id", flush_id)
                .finish(),
            LiveRepairState::Swapping => panic!("saw transient state"),
        }
    }
}

#[derive(Debug)]
struct LiveRepairData {
    /// Total number of extents that need checking
    extent_count: u64,

    /// Extent being repaired
    active_extent: u64,

    /// Minimum job ID the downstairs under repair needs to consider for deps
    min_id: JobId,

    /// Reserved live-repair job IDs
    ///
    /// If, while running live repair, we have an IO that spans repaired extents
    /// and not yet repaired extents, we will reserve job IDs for the future
    /// repair work and store them in this hash map.  When it comes time to
    /// start a repair and allocate the job IDs we will require, we first check
    /// this hash map to see if the IDs were already repaired.
    ///
    /// The key is the extent being repaired; the value is a tuple of reserved
    /// IDs and existing dependencies for the first job in the repair.
    repair_job_ids: BTreeMap<u64, (ExtentRepairIDs, Vec<JobId>)>,

    /// Downstairs providing repair info
    source_downstairs: ClientId,

    /// Downstairs being repaired
    repair_downstairs: Vec<ClientId>,

    /// Repair is being aborted
    ///
    /// This means that operations are replaced with `LiveRepairNoOp` (so that
    /// previously submitted reopen operations that depend on them will still
    /// happen); we'll stop repairing after the current extent is handled and
    /// jump straight to the final flush.
    aborting_repair: bool,

    /// Current state
    state: LiveRepairState,
}

#[derive(Debug)]
pub(crate) enum DownstairsAction {
    /// We received a client action from the given client
    Client {
        client_id: ClientId,
        action: ClientAction,
    },

    /// The currently-awaited `LiveRepair` job has returned a result
    LiveRepair(Result<(), CrucibleError>),
}

impl Downstairs {
    pub(crate) fn new(
        cfg: Arc<UpstairsConfig>,
        ds_target: ClientMap<SocketAddr>,
        tls_context: Option<Arc<crucible_common::x509::TLSContext>>,
        log: Logger,
    ) -> Self {
        let mut clients = [None, None, None];
        for i in ClientId::iter() {
            clients[i.get() as usize] = Some(DownstairsClient::new(
                i,
                cfg.clone(),
                ds_target.get(&i).copied(),
                log.new(o!("client" => i.get().to_string())),
                tls_context.clone(),
            ));
        }
        let clients = clients.map(Option::unwrap);
        Self {
            clients: ClientData(clients),
            cfg,
            next_flush: 0,
            ds_active: ActiveJobs::new(),
            write_bytes_outstanding: 0,
            completed: AllocRingBuffer::new(2048),
            completed_jobs: AllocRingBuffer::new(8),
            next_id: JobId(1000),
            reconcile_current_work: None,
            reconcile_task_list: VecDeque::new(),
            reconcile_repaired: 0,
            reconcile_repair_needed: 0,
            log: log.new(o!("" => "downstairs".to_string())),
            ackable_work: BTreeSet::new(),
            repair: None,
        }
    }

    /// Build a `Downstairs` for simple tests
    ///
    /// Note that this `Downstairs` does not have valid socket addresses, so the
    /// client tasks won't start!
    #[cfg(test)]
    pub fn test_default() -> Self {
        let log = crucible_common::build_logger();
        let cfg = Arc::new(UpstairsConfig {
            upstairs_id: Uuid::new_v4(),
            read_only: false,
            encryption_context: None,
            lossy: false,
            session_id: Uuid::new_v4(),
        });

        Self::new(cfg, ClientMap::new(), None, log)
    }

    /// Choose which `DownstairsAction` to apply
    ///
    /// This function is called from within a top-level `select!`, so not only
    /// must the select expressions be cancel safe, but the **bodies** must also
    /// be cancel-safe.  This is why we simply return a single value in the body
    /// of each statement.
    pub(crate) async fn select(&mut self) -> DownstairsAction {
        // Split borrow of the clients
        let [ca, cb, cc] = &mut self.clients.0;
        tokio::select! {
            action = ca.select() => {
                DownstairsAction::Client {
                    client_id: ClientId::new(0),
                    action
                }
            }
            action = cb.select() => {
                DownstairsAction::Client {
                    client_id: ClientId::new(1),
                    action
                }
            }
            action = cc.select() => {
                DownstairsAction::Client {
                    client_id: ClientId::new(2),
                    action
                }
            }
            r = async {
                if let Some(r) = self.repair.as_mut() {
                    // Each repair task is waiting on a single BlockReqWaiter,
                    // which is handled by the `process_io_completion` pipeline
                    match &mut r.state {
                        LiveRepairState::Closing { close_brw: brw, .. }
                        | LiveRepairState::Repairing { repair_brw: brw, .. }
                        | LiveRepairState::Noop { noop_brw: brw, .. }
                        | LiveRepairState::Reopening { reopen_brw: brw, .. }
                        | LiveRepairState::FinalFlush { flush_brw: brw, .. } =>
                            brw.wait_mut().await,
                        LiveRepairState::Swapping =>
                            panic!("invalid transient state"),
                    }
                } else {
                    futures::future::pending().await
                }
            } => {
                DownstairsAction::LiveRepair(r)
            }
        }
    }

    /// Checks whether we have ackable work
    pub(crate) fn has_ackable_jobs(&self) -> bool {
        !self.ackable_work.is_empty()
    }

    /// Send back acks for all jobs that are `AckReady`
    pub(crate) async fn ack_jobs(
        &mut self,
        gw: &mut GuestWork,
        up_stats: &UpStatOuter,
    ) {
        debug!(self.log, "ack_jobs called in Downstairs");

        let ack_list = std::mem::take(&mut self.ackable_work);
        let jobs_checked = ack_list.len();
        for ds_id_done in ack_list.iter() {
            self.ack_job(*ds_id_done, gw, up_stats).await;
        }
        debug!(self.log, "ack_ready handled {jobs_checked} jobs");
    }

    /// Send the ack for a single job back upstairs through `GuestWork`
    ///
    /// Update stats for the upstairs as well
    async fn ack_job(
        &mut self,
        ds_id: JobId,
        gw: &mut GuestWork,
        up_stats: &UpStatOuter,
    ) {
        debug!(self.log, "ack_jobs process {}", ds_id);

        let done = self.ds_active.get_mut(&ds_id).unwrap();
        assert!(!done.acked);

        let gw_id = done.guest_id;
        assert_eq!(done.ds_id, ds_id);

        let data = done.data.take();

        done.acked = true;
        let r = Self::result(done);
        Self::cdt_gw_work_done(done, up_stats);
        debug!(self.log, "[A] ack job {}:{}", ds_id, gw_id);

        gw.gw_ds_complete(gw_id, ds_id, data, r, &self.log).await;

        self.retire_check(ds_id);
    }

    /// Verify that we have enough valid IO results when considering all
    /// downstairs results before we send back success to the guest.
    ///
    /// During normal operations, reads can have two failures or skipps and
    /// still return valid data.
    ///
    /// During normal operations, write, write_unwritten, and flush can have one
    /// error or skip and still return success to the upstairs (though, the
    /// downstairs normally will not return error to the upstairs on W/F).
    ///
    /// For repair, we don't permit any errors, but do allow and handle the
    /// "skipped" case for IOs.  This allows us to recover if we are repairing a
    /// downstairs and one of the valid remaining downstairs goes offline.
    fn result(job: &DownstairsIO) -> Result<(), CrucibleError> {
        /*
         * TODO: this doesn't tell the Guest what the error(s) were?
         */
        let wc = job.state_count();

        let bad_job = match &job.work {
            IOop::Read { .. } => wc.error == 3,
            IOop::Write { .. } => wc.skipped + wc.error > 1,
            IOop::WriteUnwritten { .. } => wc.skipped + wc.error > 1,
            IOop::Flush { .. } => wc.skipped + wc.error > 1,
            IOop::ExtentClose {
                dependencies: _,
                extent,
            } => {
                panic!("Received illegal IOop::ExtentClose: {}", extent);
            }
            IOop::ExtentFlushClose { .. } => wc.error >= 1 || wc.skipped > 1,
            IOop::ExtentLiveRepair { .. } => wc.error >= 1 || wc.skipped > 1,
            IOop::ExtentLiveReopen { .. } => wc.error >= 1 || wc.skipped > 1,
            IOop::ExtentLiveNoOp { .. } => wc.error >= 1 || wc.skipped > 1,
        };

        if bad_job {
            Err(CrucibleError::IoError(format!(
                "{} out of 3 downstairs failed to complete this IO",
                wc.error + wc.skipped,
            )))
        } else {
            Ok(())
        }
    }

    /// Match on the `IOop` type, update stats, and fire DTrace probes
    fn cdt_gw_work_done(job: &DownstairsIO, stats: &UpStatOuter) {
        let ds_id = job.ds_id;
        let gw_id = job.guest_id;
        let io_size = job.io_size();
        match &job.work {
            IOop::Read {
                dependencies: _,
                requests: _,
            } => {
                cdt::gw__read__done!(|| (gw_id));
                stats.add_read(io_size as i64);
            }
            IOop::Write {
                dependencies: _,
                writes: _,
            } => {
                cdt::gw__write__done!(|| (gw_id));
                stats.add_write(io_size as i64);
            }
            IOop::WriteUnwritten {
                dependencies: _,
                writes: _,
            } => {
                cdt::gw__write__unwritten__done!(|| (gw_id));
                // We don't include WriteUnwritten operation in the
                // metrics for this guest.
            }
            IOop::Flush {
                dependencies: _,
                flush_number: _,
                gen_number: _,
                snapshot_details: _,
                extent_limit: _,
            } => {
                cdt::gw__flush__done!(|| (gw_id));
                stats.add_flush();
            }
            IOop::ExtentClose {
                dependencies: _,
                extent,
            } => {
                // The upstairs should never have an ExtentClose on the
                // work queue.  We will always use ExtentFlushClose as the
                // IOop, then convert to the proper Message to send to
                // each downstairs depending on the source/repair downstairs
                // values in that IOop.
                panic!(
                    "job: {} gw: {}  Received illegal IOop::ExtentClose {}",
                    ds_id, gw_id, extent,
                );
            }
            IOop::ExtentFlushClose {
                dependencies: _,
                extent,
                flush_number: _,
                gen_number: _,
                source_downstairs: _,
                repair_downstairs: _,
            } => {
                cdt::gw__close__done!(|| (gw_id, extent));
                stats.add_flush_close();
            }
            IOop::ExtentLiveRepair {
                dependencies: _,
                extent,
                source_downstairs: _,
                source_repair_address: _,
                repair_downstairs: _,
            } => {
                cdt::gw__repair__done!(|| (gw_id, extent));
                stats.add_extent_repair();
            }
            IOop::ExtentLiveNoOp { dependencies: _ } => {
                cdt::gw__noop__done!(|| (gw_id));
                stats.add_extent_noop();
            }
            IOop::ExtentLiveReopen {
                dependencies: _,
                extent,
            } => {
                cdt::gw__reopen__done!(|| (gw_id, extent));
                stats.add_extent_reopen();
            }
        }
    }

    pub(crate) async fn perform_work(&mut self, client_id: ClientId) {
        let more = self.io_send(client_id).await;
        self.clients[client_id].set_more_work(more);
    }

    pub(crate) async fn io_send(&mut self, client_id: ClientId) -> bool {
        /*
         * Build ourselves a list of all the jobs on the work hashmap that
         * have the job state for our client id in the IOState::New
         *
         * The length of this list (new work for a downstairs) can give us
         * an idea of how that downstairs is doing. If the number of jobs
         * to be submitted is too big (for some value of big) then there is
         * a problem. All sorts of back pressure information can be
         * gathered here. As (for the moment) the same task does both
         * transmit and receive, we can starve the receive side by spending
         * all our time sending work.
         *
         * This XXX is for coming back here and making a better job of
         * flow control.
         */
        let client = &mut self.clients[client_id];
        let (new_work, flow_control) = {
            let active_count = client.io_state_count.in_progress as usize;
            if active_count > MAX_ACTIVE_COUNT {
                // Can't do any work
                client.stats.flow_control += 1;
                return true;
            } else {
                let n = MAX_ACTIVE_COUNT - active_count;
                let (new_work, flow_control) = client.new_work(n);
                if flow_control {
                    client.stats.flow_control += 1;
                }
                (new_work, flow_control)
            }
        };

        /*
         * Now we have a list of all the job IDs that are new for our client id.
         * Walk this list and process each job, marking it InProgress as we
         * do the work. We do this in two loops because we can't hold the
         * lock for the hashmap while we do work, and if we release the lock
         * to do work, we would have to start over and look at all jobs in the
         * map to see if they are new.
         */
        for new_id in new_work {
            /*
             * Walk the list of work to do, update its status as in progress
             * and send the details to our downstairs.
             */
            if self.cfg.lossy && random() && random() {
                /*
                 * Requeue this work so it isn't completely lost.
                 */
                self.clients[client_id].requeue_one(new_id);
                continue;
            }

            /*
             * If in_progress returns None, it means that this job on this
             * client should be skipped.
             */
            let Some(job) = self.in_progress(new_id, client_id) else {
                continue;
            };

            let message = match job {
                IOop::Write {
                    dependencies,
                    writes,
                } => {
                    cdt::ds__write__io__start!(|| (new_id.0, client_id.get()));
                    Message::Write {
                        upstairs_id: self.cfg.upstairs_id,
                        session_id: self.cfg.session_id,
                        job_id: new_id,
                        dependencies,
                        writes,
                    }
                }
                IOop::WriteUnwritten {
                    dependencies,
                    writes,
                } => {
                    cdt::ds__write__unwritten__io__start!(|| (
                        new_id.0,
                        client_id.get()
                    ));
                    Message::WriteUnwritten {
                        upstairs_id: self.cfg.upstairs_id,
                        session_id: self.cfg.session_id,
                        job_id: new_id,
                        dependencies,
                        writes,
                    }
                }
                IOop::Flush {
                    dependencies,
                    flush_number,
                    gen_number,
                    snapshot_details,
                    extent_limit,
                } => {
                    cdt::ds__flush__io__start!(|| (new_id.0, client_id.get()));
                    Message::Flush {
                        upstairs_id: self.cfg.upstairs_id,
                        session_id: self.cfg.session_id,
                        job_id: new_id,
                        dependencies,
                        flush_number,
                        gen_number,
                        snapshot_details,
                        extent_limit,
                    }
                }
                IOop::Read {
                    dependencies,
                    requests,
                } => {
                    cdt::ds__read__io__start!(|| (new_id.0, client_id.get()));
                    Message::ReadRequest {
                        upstairs_id: self.cfg.upstairs_id,
                        session_id: self.cfg.session_id,
                        job_id: new_id,
                        dependencies,
                        requests,
                    }
                }
                IOop::ExtentClose {
                    dependencies: _,
                    extent: _,
                } => {
                    // This command should never exist on the upstairs side.
                    // We only construct it for downstairs IO, after
                    // receiving ExtentLiveClose from the upstairs.
                    panic!(
                        "[{}] Received illegal IOop::ExtentClose",
                        client_id
                    );
                }
                IOop::ExtentFlushClose {
                    dependencies,
                    extent,
                    flush_number,
                    gen_number,
                    source_downstairs: _,
                    repair_downstairs,
                } => {
                    cdt::ds__close__start!(|| (
                        new_id.0,
                        client_id.get(),
                        extent
                    ));
                    if repair_downstairs.contains(&client_id) {
                        // We are the downstairs being repaired, so just close.
                        Message::ExtentLiveClose {
                            upstairs_id: self.cfg.upstairs_id,
                            session_id: self.cfg.session_id,
                            job_id: new_id,
                            dependencies,
                            extent_id: extent,
                        }
                    } else {
                        Message::ExtentLiveFlushClose {
                            upstairs_id: self.cfg.upstairs_id,
                            session_id: self.cfg.session_id,
                            job_id: new_id,
                            dependencies,
                            extent_id: extent,
                            flush_number,
                            gen_number,
                        }
                    }
                }
                IOop::ExtentLiveRepair {
                    dependencies,
                    extent,
                    source_downstairs,
                    source_repair_address,
                    repair_downstairs,
                } => {
                    cdt::ds__repair__start!(|| (
                        new_id.0,
                        client_id.get(),
                        extent
                    ));
                    if repair_downstairs.contains(&client_id) {
                        Message::ExtentLiveRepair {
                            upstairs_id: self.cfg.upstairs_id,
                            session_id: self.cfg.session_id,
                            job_id: new_id,
                            dependencies,
                            extent_id: extent,
                            source_client_id: source_downstairs,
                            source_repair_address,
                        }
                    } else {
                        Message::ExtentLiveNoOp {
                            upstairs_id: self.cfg.upstairs_id,
                            session_id: self.cfg.session_id,
                            job_id: new_id,
                            dependencies,
                        }
                    }
                }
                IOop::ExtentLiveReopen {
                    dependencies,
                    extent,
                } => {
                    cdt::ds__reopen__start!(|| (
                        new_id.0,
                        client_id.get(),
                        extent
                    ));
                    Message::ExtentLiveReopen {
                        upstairs_id: self.cfg.upstairs_id,
                        session_id: self.cfg.session_id,
                        job_id: new_id,
                        dependencies,
                        extent_id: extent,
                    }
                }
                IOop::ExtentLiveNoOp { dependencies } => {
                    cdt::ds__noop__start!(|| (new_id.0, client_id.get()));
                    Message::ExtentLiveNoOp {
                        upstairs_id: self.cfg.upstairs_id,
                        session_id: self.cfg.session_id,
                        job_id: new_id,
                        dependencies,
                    }
                }
            };
            self.clients[client_id].send(message).await
        }
        flow_control
    }

    /// Mark this request as in progress for this client, and return the
    /// relevant [`IOOp`] with updated dependencies.
    ///
    /// If the job state is already [`IOState::Skipped`], then this task
    /// has no work to do, so return `None`.
    fn in_progress(
        &mut self,
        ds_id: JobId,
        client_id: ClientId,
    ) -> Option<IOop> {
        let Some(job) = self.ds_active.get_mut(&ds_id) else {
            // This job, that we thought was good, is not.  As we don't
            // keep the lock when gathering job IDs to work on, it is
            // possible to have a out of date work list.
            warn!(self.log, "[{client_id}] Job {ds_id} not on active list");
            return None;
        };

        // If current state is Skipped, then we have nothing to do here.
        if matches!(job.state[client_id], IOState::Skipped) {
            return None;
        }

        Some(
            self.clients[client_id]
                .in_progress(job, self.repair.as_ref().map(|r| r.min_id)),
        )
    }

    /// Reinitialize the given client
    pub(crate) fn reinitialize(
        &mut self,
        client_id: ClientId,
        auto_promote: Option<u64>,
    ) {
        // Restart the IO task
        self.clients[client_id].reinitialize(auto_promote);

        // If this client is coming back from being offline, then replay all of
        // its jobs.
        if self.clients[client_id].state() == DsState::Offline {
            self.replay_jobs(client_id);
        }
    }

    /// Tries to deactivate all of the Downstairs clients
    ///
    /// Returns true if we succeeded; otherwise returns false
    pub(crate) fn set_deactivate(&mut self) -> bool {
        /*
         * If any downstairs are currently offline, then we are going
         * to lock the door behind them and not let them back in until
         * all non-offline downstairs have deactivated themselves.
         *
         * However: TODO: This is not done yet.
         */
        let offline_ds: Vec<ClientId> = self
            .clients
            .iter()
            .enumerate()
            .filter(|(_i, c)| c.state() == DsState::Offline)
            .map(|(i, _c)| ClientId::new(i as u8))
            .collect();

        /*
         * TODO: Handle deactivation when a downstairs is offline.
         */
        if !offline_ds.is_empty() {
            panic!("Can't deactivate with downstairs offline (yet)");
        }

        // If there are no jobs, then we're immediately done!
        self.ds_active.is_empty()
    }

    /// Tries to deactivate the given client
    ///
    /// If successful, the client state will be set to `Deactivated` and the IO
    /// task will be stopped.  This will trigger a `ClientAction::TaskStopped`
    /// in the main task, which will in turn restart the IO task.
    ///
    /// Returns `true` on success, `false` otherwise
    pub(crate) fn try_deactivate(
        &mut self,
        client_id: ClientId,
        up_state: &UpstairsState,
    ) -> bool {
        if self.ds_active.is_empty() {
            info!(self.log, "[{}] deactivate, no work so YES", client_id);
            self.clients[client_id].deactivate(up_state);
            return true;
        }
        // If there are jobs in the queue, then we have to check them!
        let last_id = self.ds_active.keys().next_back().unwrap();

        /*
         * The last job must be a flush.  It's possible to get
         * here right after deactivating is set, but before the final
         * flush happens.
         */
        if !self.is_flush(*last_id) {
            info!(
                self.log,
                "[{}] deactivate last job {} not flush, NO", client_id, last_id
            );
            return false;
        }
        /*
         * Now count our jobs.  Any job not done or skipped means
         * we are not ready to deactivate.
         */
        for (id, job) in &self.ds_active {
            let state = &job.state[client_id];
            if state == &IOState::New || state == &IOState::InProgress {
                info!(
                    self.log,
                    "[{}] deactivate job {} not {:?} flush, NO",
                    client_id,
                    id,
                    state
                );
                return false;
            }
        }
        /*
         * To arrive here, we verified our most recent job is a flush, and
         * none of the jobs that are on our active job list are New or
         * InProgress (either error, skipped, or done)
         */
        info!(self.log, "[{}] check deactivate YES", client_id);
        self.clients[client_id].deactivate(up_state);
        true
    }

    /// Assign a new downstairs job ID.
    pub(crate) fn next_id(&mut self) -> JobId {
        let id = self.next_id;
        self.next_id.0 += 1;
        id
    }

    /// Moves all pending jobs back to the `new_jobs` queue
    ///
    /// Jobs are pending if they have not yet been flushed by this client.
    fn replay_jobs(&mut self, client_id: ClientId) {
        let lf = self.clients[client_id].last_flush();

        info!(
            self.log,
            "[{client_id}] client re-new {} jobs since flush {lf}",
            self.ds_active.len(),
        );

        self.ds_active.for_each(|ds_id, job| {
            // We don't need to send anything before our last good flush
            if *ds_id <= lf {
                assert_eq!(IOState::Done, job.state[client_id]);
                return;
            }

            self.clients[client_id].replay_job(job);
        });
    }

    /// Compare downstairs region metadata and based on the results:
    ///
    /// Determine the global flush number for this region set.
    /// Verify the guest given gen number is highest.
    /// Decide if we need repair, and if so create the repair list
    ///
    /// Returns `true` if repair is needed, `false` otherwise
    pub(crate) fn collate(
        &mut self,
        gen: u64,
        up_state: &UpstairsState,
    ) -> Result<bool, CrucibleError> {
        let r = self.collate_inner(gen);
        if r.is_err() {
            // While collating, downstairs should all be DsState::Repair.
            //
            // Any downstairs still repairing should be moved to
            // failed repair.
            assert!(self.ds_active.is_empty());
            assert!(self.reconcile_task_list.is_empty());

            for (i, c) in self.clients.iter_mut().enumerate() {
                match c.state() {
                    DsState::WaitQuorum => {
                        // Set this task as FailedRepair then restart it
                        c.set_failed_repair(up_state);
                        warn!(
                            self.log,
                            "Mark {i} as FAILED Collate in final check"
                        );
                    }
                    s => {
                        warn!(
                            self.log,
                            "downstairs in state {s} after failed collate"
                        );
                    }
                }
            }
        }
        r
    }

    fn collate_inner(&mut self, gen: u64) -> Result<bool, CrucibleError> {
        /*
         * Show some (or all if small) of the info from each region.
         *
         * This loop is load bearing, we use this loop to get the
         * max flush number.  Eventually the max flush will come after
         * we have done any repair we need to do.  Since we don't have
         * that code yet, we are making use of this loop to find our
         * max.
         */
        let mut max_flush = 0;
        let mut max_gen = 0;
        for (cid, rec) in self
            .clients
            .iter()
            .map(|c| c.region_metadata.as_ref().unwrap())
            .enumerate()
        {
            let mf = rec.flush_numbers.iter().max().unwrap() + 1;
            if mf > max_flush {
                max_flush = mf;
            }
            let mg = rec.generation.iter().max().unwrap() + 1;
            if mg > max_gen {
                max_gen = mg;
            }
            if rec.flush_numbers.len() > 12 {
                info!(
                    self.log,
                    "[{}]R flush_numbers[0..12]: {:?}",
                    cid,
                    rec.flush_numbers[0..12].to_vec()
                );
                info!(
                    self.log,
                    "[{}]R generation[0..12]: {:?}",
                    cid,
                    rec.generation[0..12].to_vec()
                );
                info!(
                    self.log,
                    "[{}]R dirty[0..12]: {:?}",
                    cid,
                    rec.dirty[0..12].to_vec()
                );
            } else {
                info!(
                    self.log,
                    "[{}]R  flush_numbers: {:?}", cid, rec.flush_numbers
                );
                info!(self.log, "[{}]R  generation: {:?}", cid, rec.generation);
                info!(self.log, "[{}]R  dirty: {:?}", cid, rec.dirty);
            }
        }

        info!(self.log, "Max found gen is {}", max_gen);
        /*
         * Verify that the generation number that the guest has requested
         * is higher than what we have from the three downstairs.
         */
        let requested_gen = gen;
        if requested_gen == 0 {
            error!(self.log, "generation number should be at least 1");
            return Err(CrucibleError::GenerationNumberTooLow(
                "Generation 0 illegal".to_owned(),
            ));
        } else if requested_gen < max_gen {
            /*
             * We refuse to connect. The provided generation number is not
             * high enough to let us connect to these downstairs.
             */
            error!(
                self.log,
                "found generation number {}, larger than requested: {}",
                max_gen,
                requested_gen,
            );
            return Err(CrucibleError::GenerationNumberTooLow(format!(
                "found generation number {}, larger than requested: {}",
                max_gen, requested_gen,
            )));
        } else {
            info!(
                self.log,
                "Generation requested: {} >= found:{}", requested_gen, max_gen,
            );
        }

        /*
         * Set the next flush ID so we have if we need to repair.
         */
        self.next_flush = max_flush;
        info!(self.log, "Next flush: {}", max_flush);

        /*
         * Determine what extents don't match and what to do
         * about that
         */
        if let Some(reconcile_list) = self.mismatch_list() {
            for c in self.clients.iter_mut() {
                c.begin_reconcile();
            }

            info!(
                self.log,
                "Found {:?} extents that need repair",
                reconcile_list.mend.len()
            );
            self.convert_rc_to_messages(
                reconcile_list.mend,
                max_flush,
                max_gen,
            );
            self.reconcile_repair_needed = self.reconcile_task_list.len();
            Ok(true)
        } else {
            info!(self.log, "All extents match");
            Ok(false)
        }
    }

    /// Tries to start live-repair
    ///
    /// Returns true on success, false otherwise
    pub(crate) fn start_live_repair(
        &mut self,
        up_state: &UpstairsState,
        gw: &mut GuestWork,
        extent_count: u64,
        generation: u64,
    ) -> bool {
        // If a live repair was in progress and encountered an error, that
        // downstairs itself will be marked Faulted.  It is possible for
        // that downstairs to reconnect and get back to LiveRepairReady
        // and be requesting for a repair before the repair task has wrapped
        // up the failed repair that this downstairs was part of.  For that
        // situation, let the repair finish and retry this repair request.
        if self.repair.is_some() {
            // TODO should the return value be a Result<bool, ??> instead?
            return false;
        }

        // Move the upstairs that were LiveRepairReady to LiveRepair
        //
        // After this point, we must call `abort_repair` if something goes wrong
        // to abort the repair on the troublesome clients.
        for c in self.clients.iter_mut() {
            c.start_live_repair(up_state);
        }

        // Begin setting up live-repair state
        assert!(self.clients.iter().all(|c| c.extent_limit.is_none()));

        let mut repair_downstairs = vec![];
        let mut source_downstairs = None;
        for cid in ClientId::iter() {
            match self.clients[cid].state() {
                DsState::LiveRepair => {
                    repair_downstairs.push(cid);
                }
                DsState::Active => {
                    source_downstairs = Some(cid);
                }
                state => {
                    warn!(
                        self.log,
                        "Unknown repair action for ds:{} in state {}",
                        cid,
                        state,
                    );
                    // TODO, what other states are okay?
                }
            }
        }

        let Some(source_downstairs) = source_downstairs else {
            error!(self.log, "failed to find source downstairs for repair");
            self.abort_repair(up_state);
            return false;
        };

        if repair_downstairs.is_empty() {
            error!(self.log, "failed to find a downstairs needing repair");
            self.abort_repair(up_state);
            return false;
        }

        // Submit the initial repair jobs, which kicks everything off
        let state = self.begin_repair_for(
            0,
            false,
            &repair_downstairs,
            source_downstairs,
            up_state,
            gw,
            generation,
        );
        let LiveRepairState::Closing { close_id, .. } = &state else {
            panic!("invalid response from `begin_repair_for`");
        };

        self.repair = Some(LiveRepairData {
            extent_count,
            repair_downstairs,
            source_downstairs,
            aborting_repair: false,
            active_extent: 0,
            min_id: *close_id,
            repair_job_ids: BTreeMap::new(),
            state,
        });

        // We'll be back in on_live_repair once the initial job finishes
        true
    }

    pub(crate) fn on_live_repair(
        &mut self,
        r: Result<(), CrucibleError>,
        gw: &mut GuestWork,
        up_state: &UpstairsState,
        generation: u64,
    ) {
        // Take the value out of `self.repair` to simplify borrow-checking
        // later.  Remember to put it back!
        let Some(mut repair) = self.repair.take() else {
            warn!(self.log, "ignoring repair result when out of LiveRepair");
            return;
        };

        match &r {
            Ok(()) => {
                // keep going
                info!(self.log, "got repair ok in {:?}", repair.state);
            }
            Err(e) => {
                error!(self.log, "got repair error {e} in {:?}", repair.state);
                // We keep going here, because we need to submit no-op jobs to
                // avoid things getting clogged up.
                repair.aborting_repair = true;
            }
        }

        // TODO check Downstairs state here?

        repair.state = match std::mem::replace(
            &mut repair.state,
            LiveRepairState::Swapping,
        ) {
            LiveRepairState::Closing {
                close_id,
                repair_id,
                noop_id,
                reopen_id,

                gw_repair_id,
                gw_noop_id,

                close_brw: _, // already consumed!
                reopen_brw,
            } => {
                // TODO check that the job is done?
                let repair_brw = if repair.aborting_repair {
                    self.create_and_enqueue_noop_io(
                        gw,
                        vec![close_id],
                        repair_id,
                        gw_repair_id,
                    )
                } else {
                    self.create_and_enqueue_repair_io(
                        gw,
                        repair.active_extent,
                        vec![close_id],
                        repair_id,
                        gw_repair_id,
                        repair.source_downstairs,
                        &repair.repair_downstairs,
                    )
                };

                info!(
                    self.log,
                    "RE:{} Wait for result from repair command {}:{}",
                    repair.active_extent,
                    repair_id,
                    gw_repair_id
                );
                LiveRepairState::Repairing {
                    repair_id,
                    noop_id,
                    reopen_id,

                    gw_noop_id,

                    repair_brw,
                    reopen_brw,
                }
            }
            LiveRepairState::Repairing {
                repair_id,
                noop_id,
                reopen_id,
                gw_noop_id,
                repair_brw: _,
                reopen_brw,
            } => {
                let noop_brw = self.create_and_enqueue_noop_io(
                    gw,
                    vec![repair_id],
                    noop_id,
                    gw_noop_id,
                );
                info!(
                    self.log,
                    "RE:{} Wait for result from NoOp command {}:{}",
                    repair.active_extent,
                    noop_id,
                    gw_noop_id
                );
                LiveRepairState::Noop {
                    noop_id,
                    reopen_id,
                    noop_brw,
                    reopen_brw,
                }
            }
            LiveRepairState::Noop {
                noop_id: _,
                reopen_id,
                noop_brw: _,
                reopen_brw,
            } => {
                info!(
                    self.log,
                    "RE:{} Wait for result from reopen command {}",
                    repair.active_extent,
                    reopen_id,
                );
                LiveRepairState::Reopening {
                    reopen_id,
                    reopen_brw,
                }
            }
            LiveRepairState::Reopening { .. } => {
                // We've finished this extent, prepare to start the next one
                repair.active_extent += 1;

                // It's possible that we've reached the end of our extents!
                let finished = repair.active_extent == repair.extent_count;

                // If we have reserved jobs for this extent, then we have to
                // keep doing (sending no-ops) because otherwise dependencies
                // will never be resolved.
                let have_reserved_jobs =
                    repair.repair_job_ids.contains_key(&repair.active_extent);

                if finished || (repair.aborting_repair && !have_reserved_jobs) {
                    // We're done, submit a final flush!
                    let (send, recv) = oneshot::channel();
                    let op = BlockOp::Flush {
                        snapshot_details: None,
                    };
                    let flush_br = BlockReq::new(op, send);
                    let flush_brw = BlockReqWaiter::new(recv);

                    let gw_id: u64 = gw.next_gw_id();
                    cdt::gw__flush__start!(|| (gw_id));

                    let flush_id = self.submit_flush(gw_id, generation, None);
                    info!(self.log, "LiveRepair final flush submitted");

                    let new_gtos = GtoS::new(flush_id, None, Some(flush_br));
                    gw.active.insert(gw_id, new_gtos);

                    cdt::up__to__ds__flush__start!(|| (gw_id));

                    LiveRepairState::FinalFlush {
                        flush_id,
                        flush_brw,
                    }
                } else {
                    self.begin_repair_for(
                        repair.active_extent,
                        repair.aborting_repair,
                        &repair.repair_downstairs,
                        repair.source_downstairs,
                        up_state,
                        gw,
                        generation,
                    )
                }
            }
            LiveRepairState::FinalFlush { .. } => {
                info!(self.log, "LiveRepair final flush returned {r:?}");
                if repair.aborting_repair {
                    warn!(self.log, "aborting live-repair");
                    self.abort_repair(up_state);
                    return;
                } else {
                    info!(self.log, "live-repair completed successfully");
                    for c in repair.repair_downstairs {
                        self.clients[c].finish_repair(up_state);
                    }
                    return;
                }
            }

            LiveRepairState::Swapping => panic!("saw intermediate state"),
        };

        self.repair = Some(repair);
    }

    fn create_and_enqueue_noop_io(
        &mut self,
        gw: &mut GuestWork,
        deps: Vec<JobId>,
        noop_id: JobId,
        gw_noop_id: u64,
    ) -> BlockReqWaiter {
        let nio = Self::create_noop_io(noop_id, deps, gw_noop_id);

        cdt::gw__noop__start!(|| (gw_noop_id));
        let (send, recv) = oneshot::channel();
        let op = BlockOp::RepairOp;
        let noop_br = BlockReq::new(op, send);
        let noop_brw = BlockReqWaiter::new(recv);
        let new_gtos = GtoS::new(noop_id, None, Some(noop_br));
        gw.active.insert(gw_noop_id, new_gtos);
        self.enqueue_repair(nio);
        noop_brw
    }

    fn create_noop_io(
        ds_id: JobId,
        dependencies: Vec<JobId>,
        gw_id: u64,
    ) -> DownstairsIO {
        let noop_ioop = IOop::ExtentLiveNoOp { dependencies };

        DownstairsIO {
            ds_id,
            guest_id: gw_id,
            work: noop_ioop,
            state: ClientData::new(IOState::New),
            acked: false,
            replay: false,
            data: None,
            read_response_hashes: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_and_enqueue_repair_io(
        &mut self,
        gw: &mut GuestWork,
        eid: u64,
        deps: Vec<JobId>,
        repair_id: JobId,
        gw_repair_id: u64,
        source: ClientId,
        repair: &[ClientId],
    ) -> BlockReqWaiter {
        let repair_io = self.repair_or_noop(
            eid as usize,
            repair_id,
            deps,
            gw_repair_id,
            source,
            repair,
        );

        cdt::gw__repair__start!(|| (gw_repair_id, eid));
        let (send, recv) = oneshot::channel();
        let op = BlockOp::RepairOp;
        let repair_br = BlockReq::new(op, send);
        let repair_brw = BlockReqWaiter::new(recv);
        let new_gtos = GtoS::new(repair_id, None, Some(repair_br));
        gw.active.insert(gw_repair_id, new_gtos);
        self.enqueue_repair(repair_io);
        repair_brw
    }

    fn repair_or_noop(
        &mut self,
        extent: usize,
        repair_id: JobId,
        repair_deps: Vec<JobId>,
        gw_repair_id: u64,
        source: ClientId,
        repair: &[ClientId],
    ) -> DownstairsIO {
        assert!(repair.len() < 3);

        let mut need_repair = Vec::new();
        debug!(self.log, "Get repair info for {} source", source);
        let good_ei = self.clients[source].repair_info.take().unwrap();
        for broken_extent in repair.iter() {
            // TODO: should this implementation be in DownstairsClient?
            debug!(self.log, "Get repair info for {} bad", broken_extent);
            let repair_ei =
                self.clients[*broken_extent].repair_info.take().unwrap();

            let repair = if repair_ei.dirty
                || repair_ei.generation != good_ei.generation
            {
                true
            } else {
                repair_ei.flush_number != good_ei.flush_number
            };

            if repair {
                need_repair.push(*broken_extent);
            }
        }

        // Now that we have consumed the contents, be sure to clear
        // out anything we did not look at.  There could be something left
        for c in self.clients.iter_mut() {
            c.repair_info = None;
        }

        if need_repair.is_empty() {
            info!(self.log, "No repair needed for extent {}", extent);
            for &cid in repair.iter() {
                self.clients[cid].stats.extents_confirmed += 1;
            }
            Self::create_noop_io(repair_id, repair_deps, gw_repair_id)
        } else {
            info!(
                self.log,
                "Repair for extent {} s:{} d:{:?}", extent, source, need_repair
            );
            for &cid in repair.iter() {
                self.clients[cid].stats.extents_repaired += 1;
            }
            let repair_address = self.clients[source].repair_addr.unwrap();

            Self::create_repair_io(
                repair_id,
                repair_deps,
                gw_repair_id,
                extent,
                repair_address,
                source,
                need_repair,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_repair_io(
        ds_id: JobId,
        dependencies: Vec<JobId>,
        gw_id: u64,
        extent: usize,
        repair_address: SocketAddr,
        source_downstairs: ClientId,
        repair_downstairs: Vec<ClientId>,
    ) -> DownstairsIO {
        let repair_ioop = IOop::ExtentLiveRepair {
            dependencies,
            extent,
            source_downstairs,
            source_repair_address: repair_address,
            repair_downstairs,
        };

        DownstairsIO {
            ds_id,
            guest_id: gw_id,
            work: repair_ioop,
            state: ClientData::new(IOState::New),
            acked: false,
            replay: false,
            data: None,
            read_response_hashes: Vec::new(),
        }
    }

    /// Begins live-repair for the given extent
    ///
    /// Claims initial IDs and submits initial jobs, then returns a
    /// `LiveRepairState::Closing`.
    ///
    /// If `aborting` is true, then all of the submitted jobs are no-ops.
    ///
    /// # Panics
    /// If the upstairs is not in `UpstairsState::Active`, the source downstairs
    /// is not `DsState::Active`, or the repair downstairs are not all
    /// `DsState::LiveRepair`.
    #[allow(clippy::too_many_arguments)]
    fn begin_repair_for(
        &mut self,
        extent: u64,
        aborting: bool,
        repair_downstairs: &[ClientId],
        source_downstairs: ClientId,
        up_state: &UpstairsState,
        gw: &mut GuestWork,
        generation: u64,
    ) -> LiveRepairState {
        // Invariant checking to begin
        assert!(
            matches!(up_state, UpstairsState::Active),
            "bad upstairs state: {up_state:?}"
        );
        assert_eq!(self.clients[source_downstairs].state(), DsState::Active);
        for &c in repair_downstairs {
            assert_eq!(self.clients[c].state(), DsState::LiveRepair);
            // We should be walking up the extents one at a time
            if extent > 0 {
                assert_eq!(self.clients[c].extent_limit, Some(extent - 1));
            } else {
                assert!(self.clients[c].extent_limit.is_none());
            }
        }

        // TODO: track this in LiveRepairState, since it's always the same?
        for &c in repair_downstairs {
            self.clients[c].extent_limit = Some(extent);
        }

        let gw_close_id: u64 = gw.next_gw_id();
        let gw_repair_id: u64 = gw.next_gw_id();
        let gw_noop_id: u64 = gw.next_gw_id();
        let gw_reopen_id: u64 = gw.next_gw_id();

        // The work IDs for the downstairs side of things.
        let (extent_repair_ids, close_deps) = self.get_repair_ids(extent);
        let close_id = extent_repair_ids.close_id;
        let repair_id = extent_repair_ids.repair_id;
        let noop_id = extent_repair_ids.noop_id;
        let reopen_id = extent_repair_ids.reopen_id;

        //  Note that `close_deps` (the list of dependencies) will potentially
        //  include skipped jobs for some downstairs.  The list of dependencies
        //  can be further altered when we are about to send IO to an individual
        //  downstairs that is under repair. At that time, we go through the
        //  list of dependencies and remove jobs that we skipped or finished for
        //  that specific downstairs before we send the repair IO over the wire.

        info!(
            self.log,
            "RE:{} repair extent with ids {},{},{},{} deps:{:?}",
            extent,
            close_id,
            repair_id,
            noop_id,
            reopen_id,
            close_deps,
        );

        let reopen_brw = if aborting {
            self.create_and_enqueue_noop_io(
                gw,
                vec![noop_id],
                reopen_id,
                gw_reopen_id,
            )
        } else {
            self.create_and_enqueue_reopen_io(
                gw,
                extent,
                vec![noop_id],
                reopen_id,
                gw_reopen_id,
            )
        };

        let close_brw = if aborting {
            self.create_and_enqueue_noop_io(
                gw,
                close_deps,
                close_id,
                gw_close_id,
            )
        } else {
            self.create_and_enqueue_close_io(
                gw,
                extent,
                generation,
                close_deps,
                close_id,
                gw_close_id,
                source_downstairs,
                repair_downstairs,
            )
        };

        LiveRepairState::Closing {
            close_id,
            repair_id,
            reopen_id,
            noop_id,

            gw_noop_id,
            gw_repair_id,

            close_brw,
            reopen_brw,
        }
    }

    fn create_reopen_io(
        eid: usize,
        ds_id: JobId,
        dependencies: Vec<JobId>,
        gw_id: u64,
    ) -> DownstairsIO {
        let reopen_ioop = IOop::ExtentLiveReopen {
            dependencies,
            extent: eid,
        };

        DownstairsIO {
            ds_id,
            guest_id: gw_id,
            work: reopen_ioop,
            state: ClientData::new(IOState::New),
            acked: false,
            replay: false,
            data: None,
            read_response_hashes: Vec::new(),
        }
    }

    fn create_and_enqueue_reopen_io(
        &mut self,
        gw: &mut GuestWork,
        eid: u64,
        deps: Vec<JobId>,
        reopen_id: JobId,
        gw_reopen_id: u64,
    ) -> BlockReqWaiter {
        let reopen_io =
            Self::create_reopen_io(eid as usize, reopen_id, deps, gw_reopen_id);

        cdt::gw__reopen__start!(|| (gw_reopen_id, eid));
        let (send, recv) = oneshot::channel();
        let op = BlockOp::RepairOp;
        let reopen_br = BlockReq::new(op, send);
        let reopen_brw = BlockReqWaiter::new(recv);
        let new_gtos = GtoS::new(reopen_id, None, Some(reopen_br));
        gw.active.insert(gw_reopen_id, new_gtos);
        self.enqueue_repair(reopen_io);
        reopen_brw
    }

    #[cfg(test)]
    pub(crate) fn create_read_eob(
        &mut self,
        ds_id: JobId,
        blocks: ImpactedBlocks,
        gw_id: u64,
        requests: Vec<ReadRequest>,
    ) -> DownstairsIO {
        let dependencies = self.ds_active.deps_for_read(ds_id, blocks);
        debug!(self.log, "IO Read  {} has deps {:?}", ds_id, dependencies);

        let aread = IOop::Read {
            dependencies,
            requests,
        };

        DownstairsIO {
            ds_id,
            guest_id: gw_id,
            work: aread,
            state: ClientData::new(IOState::New),
            acked: false,
            replay: false,
            data: None,
            read_response_hashes: Vec::new(),
        }
    }

    #[cfg(test)]
    fn create_write_eob(
        &mut self,
        ds_id: JobId,
        blocks: ImpactedBlocks,
        gw_id: u64,
        writes: Vec<crucible_protocol::Write>,
        is_write_unwritten: bool,
    ) -> DownstairsIO {
        let dependencies = self.ds_active.deps_for_write(ds_id, blocks);
        cdt::gw__write__deps!(|| (
            self.ds_active.len() as u64,
            dependencies.len() as u64
        ));
        debug!(self.log, "IO Write {} has deps {:?}", ds_id, dependencies);

        let awrite = if is_write_unwritten {
            IOop::WriteUnwritten {
                dependencies,
                writes,
            }
        } else {
            IOop::Write {
                dependencies,
                writes,
            }
        };

        DownstairsIO {
            ds_id,
            guest_id: gw_id,
            work: awrite,
            state: ClientData::new(IOState::New),
            acked: false,
            replay: false,
            data: None,
            read_response_hashes: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_close_io(
        &mut self,
        eid: usize,
        ds_id: JobId,
        dependencies: Vec<JobId>,
        gw_id: u64,
        gen: u64,
        source: ClientId,
        repair: Vec<ClientId>,
    ) -> DownstairsIO {
        let close_ioop = IOop::ExtentFlushClose {
            dependencies,
            extent: eid,
            flush_number: self.next_flush_id(),
            gen_number: gen,
            source_downstairs: source,
            repair_downstairs: repair,
        };

        DownstairsIO {
            ds_id,
            guest_id: gw_id,
            work: close_ioop,
            state: ClientData::new(IOState::New),
            acked: false,
            replay: false,
            data: None,
            read_response_hashes: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_and_enqueue_close_io(
        &mut self,
        gw: &mut GuestWork,
        eid: u64,
        gen: u64,
        deps: Vec<JobId>,
        close_id: JobId,
        gw_close_id: u64,
        source: ClientId,
        repair: &[ClientId],
    ) -> BlockReqWaiter {
        let close_io = self.create_close_io(
            eid as usize,
            close_id,
            deps,
            gw_close_id,
            gen,
            source,
            repair.to_vec(),
        );

        cdt::gw__close__start!(|| (gw_close_id, eid));
        let (send, recv) = oneshot::channel();
        let op = BlockOp::RepairOp;
        let close_br = BlockReq::new(op, send);
        let close_brw = BlockReqWaiter::new(recv);
        let new_gtos = GtoS::new(close_id, None, Some(close_br));
        gw.active.insert(gw_close_id, new_gtos);
        self.enqueue_repair(close_io);
        close_brw
    }

    /// Get the repair IDs and dependencies for this extent.
    ///
    /// If they were already reserved, then use those values (removing them from
    /// the list of reserved IDs), otherwise, go get the next set of job IDs.
    fn get_repair_ids(&mut self, eid: u64) -> (ExtentRepairIDs, Vec<JobId>) {
        if let Some((eri, deps)) = self
            .repair
            .as_mut()
            .map(|r| r.repair_job_ids.remove(&eid))
            .unwrap_or(None)
        {
            debug!(self.log, "Return existing job ids for {} GG", eid);
            (eri, deps)
        } else {
            debug!(self.log, "Create new job ids for {}", eid);
            let close_id = self.next_id();
            let repair_id = self.next_id();
            let noop_id = self.next_id();
            let reopen_id = self.next_id();
            let repair_ids = ExtentRepairIDs {
                close_id,
                repair_id,
                noop_id,
                reopen_id,
            };

            let deps = self.ds_active.deps_for_repair(repair_ids, eid);
            (repair_ids, deps)
        }
    }

    /// Returns the next flush number (post-incrementing `self.next_flush`)
    ///
    /// If we are doing a flush, the flush number and the rn number
    /// must both go up together. We don't want a lower next_id
    /// with a higher flush_number to be possible, as that can introduce
    /// dependency deadlock.
    pub(crate) fn next_flush_id(&mut self) -> u64 {
        let out = self.next_flush;
        self.next_flush += 1;
        out
    }

    /// Take a hashmap with extents we need to fix and convert that to
    /// a queue of Crucible messages we need to execute to perform the fix.
    ///
    /// The order of messages in the queue shall be the order they are
    /// performed, and no message can start until the previous message
    /// has been ack'd by all three downstairs.
    fn convert_rc_to_messages(
        &mut self,
        mut rec_list: HashMap<usize, ExtentFix>,
        max_flush: u64,
        max_gen: u64,
    ) {
        let mut rep_id = ReconciliationId(0);
        info!(self.log, "Full repair list: {:?}", rec_list);
        for (ext, ef) in rec_list.drain() {
            /*
             * For each extent needing repair, we put the following
             * tasks on the reconcile task list.
             * Flush (the source) extent with latest gen/flush#.
             * Close extent (on all ds)
             * Send repair command to bad extents
             * Reopen extent.
             */
            self.reconcile_task_list.push_back(ReconcileIO::new(
                rep_id,
                Message::ExtentFlush {
                    repair_id: rep_id,
                    extent_id: ext,
                    client_id: ef.source,
                    flush_number: max_flush,
                    gen_number: max_gen,
                },
            ));
            rep_id.0 += 1;

            self.reconcile_task_list.push_back(ReconcileIO::new(
                rep_id,
                Message::ExtentClose {
                    repair_id: rep_id,
                    extent_id: ext,
                },
            ));
            rep_id.0 += 1;

            let repair = self.clients[ef.source].repair_addr.unwrap();
            self.reconcile_task_list.push_back(ReconcileIO::new(
                rep_id,
                Message::ExtentRepair {
                    repair_id: rep_id,
                    extent_id: ext,
                    source_client_id: ef.source,
                    source_repair_address: repair,
                    dest_clients: ef.dest,
                },
            ));
            rep_id.0 += 1;

            self.reconcile_task_list.push_back(ReconcileIO::new(
                rep_id,
                Message::ExtentReopen {
                    repair_id: rep_id,
                    extent_id: ext,
                },
            ));
            rep_id.0 += 1;
        }

        info!(self.log, "Task list: {:?}", self.reconcile_task_list);
    }

    /// Takes the next task from `self.reconcile_task_list` and runs it
    ///
    /// The resulting work is stored in `self.reconcile_current_work`
    ///
    /// Returns `true` if reconciliation is complete (i.e. the task list is
    /// empty).
    ///
    /// # Panics
    /// If `self.reconcile_current_work` is not `None`
    pub(crate) async fn send_next_reconciliation_req(&mut self) -> bool {
        assert!(self.reconcile_current_work.is_none());
        let Some(mut next) = self.reconcile_task_list.pop_front() else {
            info!(self.log, "done with reconciliation");
            return true;
        };
        for c in self.clients.iter_mut() {
            c.send_next_reconciliation_req(&mut next).await;
        }
        self.reconcile_current_work = Some(next);
        false
    }

    /// Handles an ack from a repair job
    ///
    /// Returns `true` if reconciliation is complete
    ///
    /// If any of the downstairs clients are in an invalid state for
    /// reconciliation to continue, mark them as `FailedRepair` and restart
    /// them, clearing out the current reconciliation state.
    pub(crate) async fn on_reconciliation_ack(
        &mut self,
        client_id: ClientId,
        m: Message,
        up_state: &UpstairsState,
    ) -> bool {
        // Check to make sure that we're still in a repair-ready state
        //
        // If any client have dropped out of repair-readiness (e.g. due to
        // failed reconciliation, timeouts, etc), then we have to kick
        // everything else back to the beginning.
        if self.clients.iter().any(|c| c.state() != DsState::Repair) {
            // Something has changed, so abort this repair.
            // Mark any downstairs that have not changed as failed and disable
            // them so that they restart.
            self.abort_reconciliation(up_state);
            return false;
        }

        let Some(next) = self.reconcile_current_work.as_mut() else {
            // XXX what if we get a delayed ack?
            panic!("got reconciliation ack with no current work");
        };
        let Message::RepairAckId { repair_id } = m else {
            panic!("invalid message {m:?} for on_reconciliation_ack");
        };

        if self.clients[client_id].on_reconciliation_job_done(repair_id, next) {
            self.reconcile_current_work = None;
            self.send_next_reconciliation_req().await
        } else {
            false
        }
    }

    /// Handles an `ExtentError` message during reconciliation
    ///
    /// Right now, we completely abort the repair operation on all clients if
    /// this happens, and let the Upstairs sort it out once the IO tasks close.
    pub(crate) fn on_reconciliation_failed(
        &mut self,
        client_id: ClientId,
        m: Message,
        up_state: &UpstairsState,
    ) {
        let Message::ExtentError {
            repair_id,
            extent_id,
            error,
        } = m
        else {
            panic!("invalid message {m:?}");
        };
        error!(
            self.clients[client_id].log,
            "extent {extent_id} error on job {repair_id}: {error}"
        );
        self.abort_reconciliation(up_state);
    }

    fn abort_reconciliation(&mut self, up_state: &UpstairsState) {
        warn!(self.log, "aborting reconciliation");
        // Something has changed, so abort this repair.
        // Mark any downstairs that have not changed as failed and disable
        // them so that they restart.
        for (i, c) in self.clients.iter_mut().enumerate() {
            if c.state() == DsState::Repair {
                // Restart the IO task.  This will cause the Upstairs to
                // deactivate through a ClientAction::TaskStopped.
                c.set_failed_repair(up_state);
                error!(self.log, "Mark {} as FAILED REPAIR", i);
            }
        }
        info!(self.log, "Clear out existing repair work queue");
        self.reconcile_task_list = VecDeque::new();
        self.reconcile_current_work = None;
    }

    /// Asserts that initial reconciliation is done, and sets clients as Active
    ///
    /// # Panics
    /// If that isn't the case!
    pub(crate) fn on_reconciliation_done(&mut self, from_state: DsState) {
        assert!(self.ds_active.is_empty());
        assert!(self.reconcile_task_list.is_empty());

        for (i, c) in self.clients.iter_mut().enumerate() {
            assert_eq!(c.state(), from_state, "invalid state for client {i}");
            c.set_active();
        }
    }

    /// Compares region metadata from all three clients and builds a mend list
    ///
    /// # Panics
    /// If any downstairs client does not have region metadata populated
    fn mismatch_list(&self) -> Option<DownstairsMend> {
        let c = |i| {
            self.clients[ClientId::new(i)]
                .region_metadata
                .as_ref()
                .unwrap()
        };

        let log = self.log.new(o!("" => "mend".to_string()));
        DownstairsMend::new(c(0), c(1), c(2), log)
    }

    pub(crate) fn submit_flush(
        &mut self,
        gw_id: u64,
        gen: u64,
        snapshot_details: Option<SnapshotDetails>,
    ) -> JobId {
        let next_id = self.next_id();
        let flush_id = self.next_flush_id();
        let dep = self.ds_active.deps_for_flush(next_id);
        debug!(self.log, "IO Flush {} has deps {:?}", next_id, dep);

        /*
         * TODO: Walk the list of guest work structs and build the same list
         * and make sure it matches.
         */

        let extent_under_repair =
            self.get_extent_under_repair().map(|v| *v.end() as usize);

        /*
         * Build the flush request, and take note of the request ID that
         * will be assigned to this new piece of work.
         */
        let fl = Self::create_flush(
            next_id,
            dep,
            flush_id,
            gw_id,
            gen,
            snapshot_details,
            extent_under_repair,
        );

        self.enqueue(fl);
        next_id
    }

    /*
     * Create a flush DownstairsIO structure.
     */
    #[allow(clippy::too_many_arguments)]
    fn create_flush(
        ds_id: JobId,
        dependencies: Vec<JobId>,
        flush_number: u64,
        guest_id: u64,
        gen_number: u64,
        snapshot_details: Option<SnapshotDetails>,
        extent_limit: Option<usize>,
    ) -> DownstairsIO {
        let flush = IOop::Flush {
            dependencies,
            flush_number,
            gen_number,
            snapshot_details,
            extent_limit,
        };

        DownstairsIO {
            ds_id,
            guest_id,
            work: flush,
            state: ClientData::new(IOState::New),
            acked: false,
            replay: false,
            data: None,
            read_response_hashes: Vec::new(),
        }
    }

    /// Reserves repair IDs if impacted blocks overlap our extent under repair
    fn check_repair_ids_for_range(&mut self, impacted_blocks: ImpactedBlocks) {
        let Some(eur) = self.get_extent_under_repair() else {
            return;
        };
        let mut future_repair = false;
        for eid in impacted_blocks.extents().into_iter().flatten() {
            if eid == *eur.start() {
                future_repair = true;
            } else if eid > *eur.start() && (eid <= *eur.end() || future_repair)
            {
                self.reserve_repair_ids_for_extent(eid);
                future_repair = true;
            }
        }
    }

    /// Reserve some job IDs for future repair work on the given extent
    ///
    /// This is a no-op if repair IDs were already reserved for this extent
    ///
    /// # Panics
    /// If we are not undergoing live-repair
    fn reserve_repair_ids_for_extent(&mut self, eid: u64) {
        if self
            .repair
            .as_mut()
            .unwrap()
            .repair_job_ids
            .contains_key(&eid)
        {
            debug!(
                self.log,
                "reserve already has existing job ids for {}", eid
            );
            return;
        }

        debug!(
            self.log,
            "Reserve Created new job ids for {}, save them", eid
        );
        let close_id = self.next_id();
        let repair_id = self.next_id();
        let noop_id = self.next_id();
        let reopen_id = self.next_id();
        let repair_ids = ExtentRepairIDs {
            close_id,
            repair_id,
            noop_id,
            reopen_id,
        };
        let deps = self.ds_active.deps_for_repair(repair_ids, eid);
        info!(
            self.log,
            "inserting repair IDs {repair_ids:?}; got dep {deps:?}"
        );
        self.repair
            .as_mut()
            .unwrap()
            .repair_job_ids
            .insert(eid, (repair_ids, deps));
    }

    /// Create and submit a read job to the three clients
    pub(crate) fn submit_read(
        &mut self,
        guest_id: u64,
        blocks: ImpactedBlocks,
        ddef: RegionDefinition,
    ) -> JobId {
        // If there is a live-repair in progress that intersects with this read,
        // then reserve job IDs for those jobs.
        self.check_repair_ids_for_range(blocks);

        /*
         * Create the tracking info for downstairs request numbers (ds_id) we
         * will create on behalf of this guest job.
         */
        let ds_id = self.next_id();

        let dependencies = self.ds_active.deps_for_read(ds_id, blocks);
        debug!(self.log, "IO Read  {} has deps {:?}", ds_id, dependencies);

        /*
         * Now create a downstairs work job for each (eid, bo) returned
         * from extent_from_offset.
         */
        let mut requests: Vec<ReadRequest> =
            Vec::with_capacity(blocks.len(&ddef));

        for (eid, offset) in blocks.blocks(&ddef) {
            requests.push(ReadRequest { eid, offset });
        }

        let aread = IOop::Read {
            dependencies,
            requests,
        };

        let io = DownstairsIO {
            ds_id,
            guest_id,
            work: aread,
            state: ClientData::new(IOState::New),
            acked: false,
            replay: false,
            data: None,
            read_response_hashes: Vec::new(),
        };

        self.enqueue(io);

        ds_id
    }

    pub(crate) fn submit_write(
        &mut self,
        guest_id: u64,
        blocks: ImpactedBlocks,
        writes: Vec<crucible_protocol::Write>,
        is_write_unwritten: bool,
    ) -> JobId {
        // If there is a live-repair in progress that intersects with this read,
        // then reserve job IDs for those jobs.
        self.check_repair_ids_for_range(blocks);

        // TODO delegate to `create_write_eob` here?
        let ds_id = self.next_id();
        let dependencies = self.ds_active.deps_for_write(ds_id, blocks);
        cdt::gw__write__deps!(|| (
            self.ds_active.len() as u64,
            dependencies.len() as u64
        ));
        debug!(self.log, "IO Write {} has deps {:?}", ds_id, dependencies);

        let awrite = if is_write_unwritten {
            IOop::WriteUnwritten {
                dependencies,
                writes,
            }
        } else {
            IOop::Write {
                dependencies,
                writes,
            }
        };
        let io = DownstairsIO {
            ds_id,
            guest_id,
            work: awrite,
            state: ClientData::new(IOState::New),
            acked: false,
            replay: false,
            data: None,
            read_response_hashes: Vec::new(),
        };
        self.enqueue(io);
        ds_id
    }

    /// Returns the most recent extent under repair, or `None`
    fn last_repair_extent(&self) -> Option<u64> {
        self.repair
            .as_ref()
            .and_then(|r| r.repair_job_ids.last_key_value().map(|(k, _)| *k))
    }

    fn enqueue(&mut self, mut io: DownstairsIO) {
        let mut skipped = 0;
        let last_repair_extent = self.last_repair_extent();

        // Send the job to each client!
        for cid in ClientId::iter() {
            let job_state =
                self.clients[cid].enqueue(&mut io, last_repair_extent);
            if matches!(job_state, IOState::Skipped) {
                skipped += 1;
            }
        }

        // If this is a write (which will be fast-acked), increment our byte
        // counter for backpressure calculations.
        match &io.work {
            IOop::Write { writes, .. }
            | IOop::WriteUnwritten { writes, .. } => {
                self.write_bytes_outstanding +=
                    writes.iter().map(|w| w.data.len() as u64).sum::<u64>();
            }
            _ => (),
        };
        let is_write = matches!(io.work, IOop::Write { .. });

        // Puts the IO onto the downstairs work queue.
        let ds_id = io.ds_id;
        self.ds_active.insert(ds_id, io);

        if skipped == 3 {
            warn!(self.log, "job {} skipped on all downstairs", &ds_id);
        }

        if skipped == 3 || is_write {
            let job = self.ds_active.get_mut(&ds_id).unwrap();
            assert!(!job.acked);
            self.ackable_work.insert(ds_id);
        }
    }

    /// Enqueue a new downstairs live repair request.
    fn enqueue_repair(&mut self, mut io: DownstairsIO) {
        // TODO explain why this is different from `enqueue`
        for cid in ClientId::iter() {
            assert_eq!(io.state[cid], IOState::New);

            let current = self.clients[cid].state();
            // If a downstairs is faulted, we can move that job directly
            // to IOState::Skipped.
            match current {
                DsState::Faulted
                | DsState::Replaced
                | DsState::Replacing
                | DsState::LiveRepairReady => {
                    // TODO can we even get here?
                    io.state.insert(cid, IOState::Skipped);
                    self.clients[cid].io_state_count.incr(&IOState::Skipped);
                    self.clients[cid].skipped_jobs.insert(io.ds_id);
                }
                _ => {
                    self.clients[cid].io_state_count.incr(&IOState::New);
                    self.clients[cid].requeue_one(io.ds_id);
                }
            }
        }
        assert!(
            matches!(
                io.work,
                IOop::ExtentLiveReopen { .. }
                    | IOop::ExtentFlushClose { .. }
                    | IOop::ExtentLiveRepair { .. }
                    | IOop::ExtentLiveNoOp { .. }
            ),
            "bad IO work: {:?}",
            io.work
        );

        let ds_id = io.ds_id;
        debug!(self.log, "Enqueue repair job {}", ds_id);
        self.ds_active.insert(ds_id, io);
    }

    /// Returns the current extent under repair (from `self.extent_limit`)
    ///
    /// # Panics
    /// If the different downstairs have different extents under repair (which
    /// is not allowed)
    fn get_extent_under_repair(&self) -> Option<std::ops::RangeInclusive<u64>> {
        let mut extent_under_repair = None;
        for cid in ClientId::iter() {
            if let Some(eur) = self.clients[cid].extent_limit {
                if extent_under_repair.is_none() {
                    extent_under_repair = Some(eur);
                } else {
                    // We only support one extent being repaired at a time
                    assert_eq!(Some(eur), extent_under_repair);
                }
            }
        }
        if let Some(eur) = extent_under_repair {
            let end = self.last_repair_extent().unwrap_or(eur);
            Some(eur..=end)
        } else {
            None
        }
    }

    pub(crate) fn replace(
        &mut self,
        id: Uuid,
        old: SocketAddr,
        new: SocketAddr,
        up_state: &UpstairsState,
    ) -> Result<ReplaceResult, CrucibleError> {
        warn!(
            self.log,
            "{id} request to replace downstairs {old} with {new}"
        );
        warn!(
            self.log,
            "{id} request to replace downstairs {old} with {new}"
        );

        // We check all targets first to not only find our current target,
        // but to be sure our new target is not an already active target
        // for a different downstairs.
        let mut new_client_id: Option<ClientId> = None;
        let mut old_client_id: Option<ClientId> = None;
        for (i, c) in self.clients.iter().enumerate() {
            if c.target_addr == Some(new) {
                new_client_id = Some(ClientId::new(i as u8));
                info!(self.log, "{id} found new target: {new} at {i}");
            }
            if c.target_addr == Some(old) {
                old_client_id = Some(ClientId::new(i as u8));
                info!(self.log, "{id} found old target: {old} at {i}");
            }
        }

        if let Some(new_client_id) = new_client_id {
            // Our new downstairs already exists.
            if old_client_id.is_some() {
                // New target is present, but old is present too, so this is not
                // a valid replacement request.
                return Err(CrucibleError::ReplaceRequestInvalid(format!(
                    "Both old {old} and new {new} targets are in use",
                )));
            }

            // We don't really know if the "old" matches what was old,
            // as that info is gone to us now, so assume it was true.
            match self.clients[new_client_id].state() {
                DsState::Replacing
                | DsState::Replaced
                | DsState::LiveRepairReady
                | DsState::LiveRepair => {
                    // These states indicate a replacement is in progress.
                    return Ok(ReplaceResult::StartedAlready);
                }
                _ => {
                    // Any other state, we assume it is done.
                    return Ok(ReplaceResult::CompletedAlready);
                }
            }
        }

        // We put the check for the old downstairs after checking for the
        // new because we want to be able to check if a replacement has
        // already happened and return status for that first.
        let Some(old_client_id) = old_client_id else {
            warn!(self.log, "{id} downstairs {old} not found");
            return Ok(ReplaceResult::Missing);
        };

        // Check for and Block a replacement if any (other) downstairs are
        // in any of these states as we don't want to take more than one
        // downstairs offline at the same time.
        for client_id in ClientId::iter() {
            if client_id == old_client_id {
                continue;
            }
            match self.clients[client_id].state() {
                DsState::Replacing
                | DsState::Replaced
                | DsState::LiveRepairReady
                | DsState::LiveRepair => {
                    return Err(CrucibleError::ReplaceRequestInvalid(format!(
                        "Replace {old} failed, downstairs {client_id} is {:?}",
                        self.clients[client_id].state(),
                    )));
                }
                _ => {}
            }
        }

        // Now we have found our old downstairs, verified the new is not in use
        // elsewhere, verified no other downstairs are in a bad state, we can
        // move forward with the replacement.
        info!(self.log, "{id} replacing old: {old} at {old_client_id}");

        // Skip all outstanding jobs for this client
        self.skip_all_jobs(old_client_id);

        // Clear the client state and restart the IO task
        self.clients[old_client_id].replace(up_state, new);

        Ok(ReplaceResult::Started)
    }

    pub(crate) fn check_gone_too_long(
        &mut self,
        client_id: ClientId,
        up_state: &UpstairsState,
    ) {
        let work_count = self.clients[client_id].total_live_work();
        if work_count > crate::IO_OUTSTANDING_MAX {
            warn!(
                self.log,
                "downstairs failed, too many outstanding jobs {}", work_count,
            );
            self.skip_all_jobs(client_id);
            self.clients[client_id]
                .fault(up_state, ClientStopReason::TooManyOutstandingJobs);
        }
    }

    /// Move all `New` and `InProgress` jobs for the given client to `Skipped`
    ///
    /// This may lead to jobs being marked as ackable, since a skipped job
    /// counts as complete in some circumstances.
    fn skip_all_jobs(&mut self, client_id: ClientId) {
        info!(
            self.log,
            "[{client_id}] client skip {} in process jobs because fault",
            self.ds_active.len(),
        );

        let mut retire_check = vec![];
        let mut number_jobs_skipped = 0;

        self.ds_active.for_each(|ds_id, job| {
            let state = &job.state[client_id];

            if matches!(state, IOState::InProgress | IOState::New) {
                self.clients[client_id].skip_job(job);
                number_jobs_skipped += 1;

                // Check to see if this being skipped means we can ACK
                // the job back to the guest.
                if job.acked {
                    // Push this onto a queue to do the retire check when
                    // we aren't doing a mutable iteration.
                    retire_check.push(*ds_id);
                } else {
                    let wc = job.state_count();
                    if (wc.error + wc.skipped + wc.done) == 3 {
                        info!(
                            self.log,
                            "[{}] notify = true for {}", client_id, ds_id
                        );
                        self.ackable_work.insert(*ds_id);
                    }
                }
            }
        });

        info!(
            self.log,
            "[{}] changed {} jobs to fault skipped",
            client_id,
            number_jobs_skipped
        );

        for ds_id in retire_check {
            self.retire_check(ds_id);
        }

        // We have eliminated all of our jobs in IOState::New above; flush
        // our cache to reflect that.
        self.clients[client_id].clear_new_jobs();

        // As this downstairs is now faulted, we clear the extent_limit.
        self.clients[client_id].extent_limit = None;
    }

    /// Aborts an in-progress live-repair
    pub(crate) fn abort_repair(&mut self, up_state: &UpstairsState) {
        assert!(self
            .clients
            .iter()
            .any(|c| c.state() == DsState::LiveRepair));
        self.repair = None;
        for i in ClientId::iter() {
            if self.clients[i].state() == DsState::LiveRepair {
                self.skip_all_jobs(i);
                self.clients[i].abort_repair(up_state);
            }
        }
    }

    /// Check if an active job is a flush or not.
    fn is_flush(&self, ds_id: JobId) -> bool {
        let job = self.ds_active.get(&ds_id).expect("checked missing job");

        matches!(job.work, IOop::Flush { .. })
    }

    /// This request is now complete on all peers, but is it ready to retire?
    /// Only when a flush is complete on all three downstairs do we check to
    /// see if we can remove jobs. Double check that all write jobs have
    /// finished and panic if not.
    ///
    /// Note we don't retire jobs until all three downstairs have returned
    /// from the same flush because the Upstairs replays all jobs since
    /// the last flush if a downstairs goes away and then comes back.
    /// This includes reads because they can be in the deps list for
    /// writes and if they aren't included in replay then the write will
    /// never start.
    fn retire_check(&mut self, ds_id: JobId) {
        if !self.is_flush(ds_id) {
            return;
        }

        // Only a completed flush will remove jobs from the active queue -
        // currently we have to keep everything around for use during replay
        let wc = self.ds_active.get(&ds_id).unwrap().state_count();
        if (wc.error + wc.skipped + wc.done) == 3 {
            assert!(!self.completed.contains(&ds_id));
            assert_eq!(wc.active, 0);

            // Retire all the jobs that happened before and including this
            // flush, with a few exceptions.  Because we can't iterate and
            // modify the list simultaneously, we mark to-be-retired jobs in
            // `retired`, then remove them in bulk after checking the list.
            let mut retired = Vec::new();

            for (&id, job) in &self.ds_active {
                if id > ds_id {
                    break;
                };
                assert!(id <= ds_id);

                // While we don't expect any jobs to still be in progress,
                // there is nothing to prevent a flush ACK from getting
                // ahead of the ACK from something that flush depends on.
                // The downstairs does handle the dependency.
                let wc = job.state_count();
                if wc.active != 0 || !job.acked {
                    warn!(
                        self.log,
                        "[rc] leave job {} on the queue when removing {} {:?}",
                        job.ds_id,
                        ds_id,
                        wc,
                    );
                    continue;
                }

                // Assert the job is actually done, then complete it
                assert_eq!(wc.error + wc.skipped + wc.done, 3);
                assert!(!self.completed.contains(&id));
                assert!(job.acked);
                assert_eq!(job.ds_id, id);

                retired.push(job.ds_id);
                self.completed.push(id);
                let summary = job.io_summarize();
                self.completed_jobs.push(summary);
                for cid in ClientId::iter() {
                    let old_state = &job.state[cid];
                    self.clients[cid].io_state_count.decr(old_state);
                }
            }
            // Now that we've collected jobs to retire, remove them from the map
            for &id in &retired {
                let job = self.ds_active.remove(&id);
                // Update pending bytes when this job is retired
                match &job.work {
                    IOop::Write { writes, .. }
                    | IOop::WriteUnwritten { writes, .. } => {
                        self.write_bytes_outstanding = self
                            .write_bytes_outstanding
                            .checked_sub(
                                writes
                                    .iter()
                                    .map(|w| w.data.len() as u64)
                                    .sum::<u64>(),
                            )
                            .unwrap();
                    }
                    _ => (),
                }
            }

            debug!(self.log, "[rc] retire {} clears {:?}", ds_id, retired);
            // Only keep track of skipped jobs at or above the flush.
            for cid in ClientId::iter() {
                self.clients[cid].skipped_jobs.retain(|&x| x >= ds_id);
            }
        }
    }

    /// Returns the number of active jobs
    pub(crate) fn active_count(&self) -> usize {
        self.ds_active.len()
    }

    /// Prints a summary of active work to `stdout`
    pub(crate) fn show_all_work(&self) {
        println!(
            "{0:>5} {1:>8} {2:>5} {3:>7} {4:>7} {5:>5} {6:>5} {7:>5} {8:>7}",
            "GW_ID",
            "ACK",
            "DSID",
            "TYPE",
            "BLOCKS",
            "DS:0",
            "DS:1",
            "DS:2",
            "REPLAY",
        );

        for (id, job) in &self.ds_active {
            let ack = if job.acked {
                AckStatus::Acked
            } else {
                AckStatus::NotAcked
            };

            let (job_type, num_blocks): (String, usize) = match &job.work {
                IOop::Read {
                    dependencies: _dependencies,
                    requests,
                } => {
                    let job_type = "Read".to_string();
                    let num_blocks = requests.len();
                    (job_type, num_blocks)
                }
                IOop::Write {
                    dependencies: _dependencies,
                    writes,
                } => {
                    let job_type = "Write".to_string();
                    let mut num_blocks = 0;

                    for write in writes {
                        let block_size = write.offset.block_size_in_bytes();
                        num_blocks += write.data.len() / block_size as usize;
                    }

                    (job_type, num_blocks)
                }
                IOop::WriteUnwritten {
                    dependencies: _dependencies,
                    writes,
                } => {
                    let job_type = "WriteU".to_string();
                    let mut num_blocks = 0;

                    for write in writes {
                        let block_size = write.offset.block_size_in_bytes();
                        num_blocks += write.data.len() / block_size as usize;
                    }

                    (job_type, num_blocks)
                }
                IOop::Flush {
                    dependencies: _dependencies,
                    flush_number: _flush_number,
                    gen_number: _gen_number,
                    snapshot_details: _,
                    extent_limit: _,
                } => {
                    let job_type = "Flush".to_string();
                    (job_type, 0)
                }
                IOop::ExtentClose {
                    dependencies: _,
                    extent,
                } => {
                    let job_type = "EClose".to_string();
                    (job_type, *extent)
                }
                IOop::ExtentFlushClose {
                    dependencies: _,
                    extent,
                    flush_number: _,
                    gen_number: _,
                    source_downstairs: _,
                    repair_downstairs: _,
                } => {
                    let job_type = "FClose".to_string();
                    (job_type, *extent)
                }
                IOop::ExtentLiveRepair {
                    dependencies: _,
                    extent,
                    source_downstairs: _,
                    source_repair_address: _,
                    repair_downstairs: _,
                } => {
                    let job_type = "Repair".to_string();
                    (job_type, *extent)
                }
                IOop::ExtentLiveReopen {
                    dependencies: _,
                    extent,
                } => {
                    let job_type = "Reopen".to_string();
                    (job_type, *extent)
                }
                IOop::ExtentLiveNoOp { dependencies: _ } => {
                    let job_type = "NoOp".to_string();
                    (job_type, 0)
                }
            };

            print!(
                "{0:>5} {1:>8} {2:>5} {3:>7} {4:>7}",
                job.guest_id, ack, id, job_type, num_blocks
            );

            for cid in ClientId::iter() {
                let state = &job.state[cid];
                // XXX I have no idea why this is two spaces instead of
                // one...
                print!("  {0:>5}", state);
            }
            print!(" {0:>6}", job.replay);

            println!();
        }
        self.io_state_count().show_all();
        print!("Last Flush: ");
        for c in self.clients.iter() {
            print!("{} ", c.last_flush());
        }
        println!();
    }

    /// Collects stats from the three `DownstairsClient`s
    pub fn collect_stats<T, F: Fn(&DownstairsClient) -> T>(
        &self,
        f: F,
    ) -> [T; 3] {
        [
            f(&self.clients[ClientId::new(0)]),
            f(&self.clients[ClientId::new(1)]),
            f(&self.clients[ClientId::new(2)]),
        ]
    }

    pub fn io_state_count(&self) -> IOStateCount {
        let d = self.collect_stats(|c| c.io_state_count);
        let f = |g: fn(ClientIOStateCount) -> u32| {
            ClientData([g(d[0]), g(d[1]), g(d[2])])
        };
        IOStateCount {
            new: f(|d| d.new),
            in_progress: f(|d| d.in_progress),
            done: f(|d| d.done),
            skipped: f(|d| d.skipped),
            error: f(|d| d.error),
        }
    }

    /// Prints the last `n` completed jobs to `stdout`
    pub(crate) fn print_last_completed(&self, n: usize) {
        // TODO this is a ringbuffer, why are we turning it to a Vec to look at
        // the last five items?
        let done = self.completed.to_vec();
        for j in done.iter().rev().take(n) {
            print!(" {:4}", j);
        }
    }

    pub(crate) fn process_io_completion(
        &mut self,
        client_id: ClientId,
        m: Message,
        up_state: &UpstairsState,
    ) -> Result<(), CrucibleError> {
        let (upstairs_id, session_id, ds_id, read_data, extent_info) = match &m
        {
            Message::WriteAck {
                upstairs_id,
                session_id,
                job_id,
                result,
            } => {
                cdt::ds__write__io__done!(|| (job_id.0, client_id.get()));
                (
                    *upstairs_id,
                    *session_id,
                    *job_id,
                    result.clone().map(|_| Vec::new()),
                    None,
                )
            }
            Message::WriteUnwrittenAck {
                upstairs_id,
                session_id,
                job_id,
                result,
            } => {
                cdt::ds__write__unwritten__io__done!(|| (
                    job_id.0,
                    client_id.get()
                ));
                (
                    *upstairs_id,
                    *session_id,
                    *job_id,
                    result.clone().map(|_| Vec::new()),
                    None,
                )
            }
            Message::FlushAck {
                upstairs_id,
                session_id,
                job_id,
                result,
            } => {
                cdt::ds__flush__io__done!(|| (job_id.0, client_id.get()));
                (
                    *upstairs_id,
                    *session_id,
                    *job_id,
                    result.clone().map(|_| Vec::new()),
                    None,
                )
            }
            Message::ReadResponse {
                upstairs_id,
                session_id,
                job_id,
                responses,
            } => {
                cdt::ds__read__io__done!(|| (job_id.0, client_id.get()));
                (*upstairs_id, *session_id, *job_id, responses.clone(), None)
            }

            Message::ExtentLiveCloseAck {
                upstairs_id,
                session_id,
                job_id,
                result,
            } => {
                // Take the result from the live close and store it away.
                let extent_info = match result {
                    Ok((g, f, d)) => {
                        debug!(
                            self.log,
                            "[{}] ELC got g:{} f:{} d:{}", client_id, g, f, d
                        );
                        Some(ExtentInfo {
                            generation: *g,
                            flush_number: *f,
                            dirty: *d,
                        })
                    }
                    Err(e) => {
                        panic!(
                            "[{}] ELC-Ack {} returned error {:?}",
                            client_id, job_id, e
                        );
                    }
                };
                cdt::ds__close__done!(|| (job_id.0, client_id.get()));
                (
                    *upstairs_id,
                    *session_id,
                    *job_id,
                    result.clone().map(|_| Vec::new()),
                    extent_info,
                )
            }
            Message::ExtentLiveAckId {
                upstairs_id,
                session_id,
                job_id,
                result,
            } => {
                cdt::ds__noop__done!(|| (job_id.0, client_id.get()));
                cdt::ds__reopen__done!(|| (job_id.0, client_id.get()));
                (
                    *upstairs_id,
                    *session_id,
                    *job_id,
                    result.clone().map(|_| Vec::new()),
                    None,
                )
            }
            Message::ExtentLiveRepairAckId {
                upstairs_id,
                session_id,
                job_id,
                result,
            } => {
                cdt::ds__repair__done!(|| (job_id.0, client_id.get()));
                (
                    *upstairs_id,
                    *session_id,
                    *job_id,
                    result.clone().map(|_| Vec::new()),
                    None,
                )
            }
            Message::ErrorReport {
                upstairs_id,
                session_id,
                job_id,
                error,
            } => {
                // The Upstairs should not consider a job completed until it has
                // returned an Ok result, and should therefore log and eat all
                // ErrorReport messages here. This will change in the future
                // when the Upstairs tracks the number of errors per Downstairs,
                // and acts on that information somehow.
                error!(
                    self.clients[client_id].log,
                    "job id {} saw error {:?}", job_id, error
                );

                // However, there is one case (see `check_message_for_abort` in
                // downstairs/src/lib.rs) where the Upstairs **does** need to
                // act: when a repair job in the Downstairs fails, that
                // Downstairs aborts itself and reconnects.
                if let Some(job) = self.ds_active.get(job_id) {
                    if job.work.is_repair() {
                        // Return the error and let the previously written error
                        // processing code work.
                        cdt::ds__repair__done!(|| (job_id.0, client_id.get()));
                        (
                            *upstairs_id,
                            *session_id,
                            *job_id,
                            Err(error.clone()),
                            None,
                        )

                        // XXX return Ok(()) here to make the upstairs stuck in
                        // test_error_during_live_repair_no_halt
                    } else {
                        return Ok(());
                    }
                } else {
                    return Ok(());
                }
            }
            m => panic!("called on_io_completion with invalid message {m:?}"),
        };

        if self.cfg.upstairs_id != upstairs_id {
            warn!(
                self.clients[client_id].log,
                "upstairs_id {:?} != job {} upstairs_id {:?}!",
                self.cfg.upstairs_id,
                ds_id,
                upstairs_id,
            );
            // TODO should these errors cause more behavior?  Right now, they're
            // only logged in the caller and then we move on.
            return Err(CrucibleError::UuidMismatch);
        }

        if self.cfg.session_id != session_id {
            warn!(
                self.clients[client_id].log,
                "u.session_id {:?} != job {} session_id {:?}!",
                self.cfg.session_id,
                ds_id,
                session_id,
            );

            return Err(CrucibleError::UuidMismatch);
        }

        /*
         * We can finish the job if the downstairs has gone away, but
         * not if it has gone away then come back, because when it comes
         * back, it will have to replay everything.
         * While the downstairs is away, it's OK to act on the result that
         * we already received, because it may never come back.
         */
        let ds_state = self.clients[client_id].state();
        match ds_state {
            DsState::Active | DsState::Repair | DsState::LiveRepair => {}
            DsState::Faulted => {
                error!(
                    self.clients[client_id].log,
                    "Dropping job {}, this downstairs is faulted", ds_id,
                );
                return Err(CrucibleError::NoLongerActive);
            }
            _ => {
                warn!(
                    self.clients[client_id].log,
                    "{} WARNING finish job {} when downstairs state: {}",
                    self.cfg.upstairs_id,
                    ds_id,
                    ds_state
                );
            }
        }

        self.process_io_completion_inner(
            ds_id,
            client_id,
            read_data,
            up_state,
            extent_info,
        );

        // Decide what to do when we have an error from this IO.
        // Mark this downstairs as bad if this was a write or flush
        match self.client_error(ds_id, client_id) {
            Some(CrucibleError::UpstairsInactive) => {
                error!(
                    self.log,
                    "Saw CrucibleError::UpstairsInactive on client {}!",
                    client_id
                );
                self.clients[client_id]
                    .checked_state_transition(up_state, DsState::Disabled);
            }
            Some(CrucibleError::DecryptionError) => {
                // We should always be able to decrypt the data.  If we
                // can't, then we have the wrong key, or the data (or key)
                // is corrupted.
                error!(
                    self.clients[client_id].log,
                    "Authenticated decryption failed on job: {:?}", ds_id
                );
                panic!(
                    "[{}] Authenticated decryption failed on job: {:?}",
                    client_id, ds_id
                );
            }
            Some(CrucibleError::SnapshotExistsAlready(_)) => {
                // This is fine, nothing to worry about
            }
            Some(_err) => {
                let Some(job) = self.ds_active.get(&ds_id) else {
                    panic!("I don't think we should be here");
                };
                if matches!(
                    job.work,
                    IOop::Write { .. }
                        | IOop::Flush { .. }
                        | IOop::WriteUnwritten { .. }
                        | IOop::ExtentFlushClose { .. }
                        | IOop::ExtentLiveRepair { .. }
                        | IOop::ExtentLiveNoOp { .. }
                        | IOop::ExtentLiveReopen { .. }
                ) {
                    // This error means the downstairs will go to Faulted.
                    // Walk the active job list and mark any that were
                    // new or in progress to skipped.
                    self.skip_all_jobs(client_id);
                    self.clients[client_id]
                        .checked_state_transition(up_state, DsState::Faulted);
                    // TODO should we restart the client task here?
                }
            }
            None => {
                // Nothing to do here, no error!
            }
        }
        Ok(())
    }

    /// Returns a client error associated with the given job
    ///
    /// Returns `None` if that job is no longer active or has no error
    /// associated with it. For example, this can happen for the third (out of
    /// three) FlushAck messages, since the job is retired upon 2/3 flushes
    /// returning okay.
    fn client_error(
        &self,
        ds_id: JobId,
        client_id: ClientId,
    ) -> Option<CrucibleError> {
        let Some(job) = self.ds_active.get(&ds_id) else {
            return None;
        };

        let state = &job.state[client_id];

        if let IOState::Error(e) = state {
            Some(e.clone())
        } else {
            None
        }
    }

    /// Wrapper for marking a single job as done from the given client
    ///
    /// This can be used to test handling of ackable work, etc.
    ///
    /// Returns true if the given job has gone from not ackable (not present in
    /// `self.ackable_work`) to ackable.  This is for historical reasons,
    /// because it's often used in existing tests.
    #[cfg(test)]
    pub fn process_ds_completion(
        &mut self,
        ds_id: JobId,
        client_id: ClientId,
        responses: Result<Vec<ReadResponse>, CrucibleError>,
        up_state: &UpstairsState,
        extent_info: Option<ExtentInfo>,
    ) -> bool {
        let was_ackable = self.ackable_work.contains(&ds_id);
        self.process_io_completion_inner(
            ds_id,
            client_id,
            responses,
            up_state,
            extent_info,
        );
        let now_ackable = self.ackable_work.contains(&ds_id);
        !was_ackable && now_ackable
        // TODO should this also ack the job, to mimick our event loop?
    }

    fn process_io_completion_inner(
        &mut self,
        ds_id: JobId,
        client_id: ClientId,
        responses: Result<Vec<ReadResponse>, CrucibleError>,
        up_state: &UpstairsState,
        extent_info: Option<ExtentInfo>,
    ) {
        /*
         * Assume we don't have enough completed jobs, and only change
         * it if we have the exact amount required
         */
        let deactivate = matches!(up_state, UpstairsState::Deactivating);

        let Some(job) = self.ds_active.get_mut(&ds_id) else {
            panic!("reqid {ds_id} is not active");
        };

        if self.clients[client_id].process_io_completion(
            job,
            responses,
            deactivate,
            extent_info,
        ) {
            self.ackable_work.insert(ds_id);
        }

        /*
         * If all 3 jobs are done, we can check here to see if we can
         * remove this job from the DS list. If we have completed the ack
         * to the guest, then there will be no more work on this job
         * but messages may still be unprocessed.
         */
        if job.acked {
            self.retire_check(ds_id);
        } else {
            // If we reach this then the job probably has errors and
            // hasn't acked back yet. We check for NotAcked so we don't
            // double count three done and return true if we already have
            // AckReady set.
            let wc = job.state_count();

            // If we are a write or a flush with one success, then
            // we must switch our state to failed.  This condition is
            // handled in Downstairs::result()
            if (wc.error + wc.skipped + wc.done) == 3 {
                self.ackable_work.insert(ds_id);
                debug!(self.log, "[{}] Set AckReady {}", client_id, job.ds_id);
            }
        }
    }

    /// Accessor for [`Downstairs::reconcile_repaired`]
    pub(crate) fn reconcile_repaired(&self) -> usize {
        self.reconcile_repaired
    }

    /// Accessor for [`Downstairs::reconcile_repair_needed`]
    pub(crate) fn reconcile_repair_needed(&self) -> usize {
        self.reconcile_repair_needed
    }

    pub(crate) fn get_work_summary(&self) -> crate::control::DownstairsWork {
        let mut kvec: Vec<_> = self.ds_active.keys().cloned().collect();
        kvec.sort_unstable();

        let mut jobs = Vec::new();
        for id in kvec.iter() {
            let job = self.ds_active.get(id).unwrap();
            let work_summary = job.io_summarize();
            jobs.push(work_summary);
        }
        let completed = self.completed_jobs.to_vec();
        crate::control::DownstairsWork { jobs, completed }
    }

    pub(crate) fn write_bytes_outstanding(&self) -> u64 {
        self.write_bytes_outstanding
    }

    /// Marks a single job as acked
    ///
    /// The job is removed from `self.ackable_work` and `acked` is set to `true`
    /// in the job's state.
    ///
    /// This is only useful in tests; in real code, we'd also want to reply to
    /// the guest when acking a job.
    #[cfg(test)]
    fn ack(&mut self, ds_id: JobId) {
        /*
         * Move AckReady to Acked.
         */
        let Some(job) = self.ds_active.get_mut(&ds_id) else {
            panic!("reqid {} is not active", ds_id);
        };
        if !self.ackable_work.remove(&ds_id) {
            panic!("Job {ds_id} is not ackable");
        }

        if job.acked {
            panic!("Job {ds_id} already acked!");
        }
        job.acked = true;
    }

    /// Returns all jobs in sorted order by [`JobId`]
    ///
    /// This function is used in unit tests for the Upstairs
    #[cfg(test)]
    pub(crate) fn get_all_jobs(&self) -> Vec<&DownstairsIO> {
        // This is a BTreeMap, so it's already sorted
        self.ds_active.values().collect()
    }

    /// Return the extent range covered by the given job
    ///
    /// # Panics
    /// If the job is not stored in our `Downstairs`
    #[cfg(test)]
    pub fn get_extents_for(&self, job: &DownstairsIO) -> ImpactedBlocks {
        self.ds_active.get_extents_for(job.ds_id)
    }
}

#[cfg(test)]
pub(crate) mod test {
    use super::Downstairs;
    use crate::{
        integrity_hash,
        test::{
            create_generic_read_eob, generic_read_request,
            generic_write_request,
        },
        upstairs::UpstairsState,
        BlockContext, ClientId, CrucibleError, EncryptionContext, ExtentFix,
        IOState, JobId, ReadResponse, ReconciliationId, SnapshotDetails,
    };
    use bytes::{Bytes, BytesMut};
    use crucible_protocol::Message;
    use ringbuffer::RingBuffer;

    use std::{
        collections::HashMap,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::Arc,
    };

    /// Forces the given job to completion and acks it
    ///
    /// This calls `Downstairs`-internal APIs and is therefore in the same
    /// module, but should not be used outside of test code.
    pub(crate) fn finish_job(ds: &mut Downstairs, ds_id: JobId) {
        for client_id in ClientId::iter() {
            ds.in_progress(ds_id, client_id);
            ds.process_ds_completion(
                ds_id,
                client_id,
                Ok(vec![]),
                &UpstairsState::Active,
                None,
            );
        }

        ds.ack(ds_id);
    }

    #[tokio::test]
    async fn work_flush_three_ok() {
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();
        let dep = ds.ds_active.deps_for_flush(next_id);

        let op = Downstairs::create_flush(next_id, dep, 10, 0, 0, None, None);

        ds.enqueue(op);

        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));
        assert_eq!(ds.ackable_work.len(), 1);
        assert!(ds.completed.is_empty());

        assert!(!ds.ds_active.get(&next_id).unwrap().acked);
        ds.ack(next_id);

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert_eq!(ds.completed.len(), 1);
    }

    // Ensure that a snapshot requires all three downstairs to return Ok
    #[tokio::test]
    async fn work_flush_snapshot_needs_three() {
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();
        let dep = ds.ds_active.deps_for_flush(next_id);
        let op = Downstairs::create_flush(
            next_id,
            dep,
            10,
            0,
            0,
            Some(SnapshotDetails {
                snapshot_name: String::from("snap"),
            }),
            None,
        );

        ds.enqueue(op);

        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));

        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));

        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));

        assert_eq!(ds.ackable_work.len(), 1);

        ds.ack(next_id);
        ds.retire_check(next_id);

        assert_eq!(ds.completed.len(), 1);
    }

    #[tokio::test]
    async fn work_flush_one_error_then_ok() {
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();
        let dep = ds.ds_active.deps_for_flush(next_id);

        let op = Downstairs::create_flush(next_id, dep, 10, 0, 0, None, None);

        ds.enqueue(op);

        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));
        assert_eq!(ds.ackable_work.len(), 1);

        ds.ack(next_id);
        ds.retire_check(next_id);

        assert_eq!(ds.completed.len(), 1);
        // No skipped jobs here.
        assert!(ds.clients[ClientId::new(0)].skipped_jobs.is_empty());
        assert!(ds.clients[ClientId::new(1)].skipped_jobs.is_empty());
        assert!(ds.clients[ClientId::new(2)].skipped_jobs.is_empty());
    }

    #[tokio::test]
    async fn work_flush_two_errors_equals_fail() {
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();
        let dep = ds.ds_active.deps_for_flush(next_id);

        let op = Downstairs::create_flush(next_id, dep, 10, 0, 0, None, None);

        ds.enqueue(op);

        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));
        assert_eq!(ds.ackable_work.len(), 1);

        ds.ack(next_id);
        ds.retire_check(next_id);

        assert_eq!(ds.completed.len(), 1);
    }

    #[tokio::test]
    async fn work_read_one_ok() {
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, next_id);

        ds.enqueue(op);

        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);

        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None,
        ));
        assert_eq!(ds.ackable_work.len(), 1);
        assert!(ds.completed.is_empty());

        ds.ack(next_id);

        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            response,
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            response,
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        // A flush is required to move work to completed
        assert!(ds.completed.is_empty());
    }

    #[tokio::test]
    async fn work_read_one_bad_two_ok() {
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, next_id);

        ds.enqueue(op);

        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);

        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            response,
            &UpstairsState::Active,
            None,
        ));
        assert_eq!(ds.ackable_work.len(), 1);
        assert!(ds.completed.is_empty());

        ds.ack(next_id);

        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            response,
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        // A flush is required to move work to completed
        // That this is still zero is part of the test
        assert!(ds.completed.is_empty());
    }

    #[tokio::test]
    async fn work_read_two_bad_one_ok() {
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, next_id);

        ds.enqueue(op);

        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);

        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            response,
            &UpstairsState::Active,
            None,
        ));
        assert_eq!(ds.ackable_work.len(), 1);

        ds.ack(next_id);
        ds.retire_check(next_id);

        // A flush is required to move work to completed
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());
    }

    #[tokio::test]
    async fn work_read_three_bad() {
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();

        let (_request, op) = create_generic_read_eob(&mut ds, next_id);

        ds.enqueue(op);

        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));
        assert_eq!(ds.ackable_work.len(), 1);

        ds.ack(next_id);
        ds.retire_check(next_id);

        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());
    }

    #[tokio::test]
    async fn work_read_two_ok_one_bad() {
        // Test that missing data on the 2nd read response will panic
        let mut ds = Downstairs::test_default();

        let (request, iblocks) = generic_read_request();
        let next_id = {
            let next_id = ds.next_id();

            let op =
                ds.create_read_eob(next_id, iblocks, 10, vec![request.clone()]);

            ds.enqueue(op);

            ds.in_progress(next_id, ClientId::new(0));
            ds.in_progress(next_id, ClientId::new(1));
            ds.in_progress(next_id, ClientId::new(2));

            next_id
        };

        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);

        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            response.clone(),
            &UpstairsState::Active,
            None,
        ));

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None,
        ));

        // emulated run in up_ds_listen
        ds.ack(next_id);
        ds.retire_check(next_id);

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));

        assert!(ds.ackable_work.is_empty());
        // Work won't be completed until we get a flush.
        assert!(ds.completed.is_empty());
    }

    #[tokio::test]
    async fn work_read_hash_mismatch() {
        // Test that a hash mismatch will trigger a panic.
        let mut ds = Downstairs::test_default();

        let id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, id);

        ds.enqueue(op);

        ds.in_progress(id, ClientId::new(0));
        ds.in_progress(id, ClientId::new(1));

        // Generate the first read response, this will be what we compare
        // future responses with.
        let r1 = Ok(vec![ReadResponse::from_request_with_data(&request, &[9])]);

        ds.process_ds_completion(
            id,
            ClientId::new(0),
            r1,
            &UpstairsState::Active,
            None,
        );

        // We must move the completed job along the process, this enables
        // process_ds_completion to know to compare future jobs to this
        // one.
        ds.ack(id);

        // Second read response, different hash
        let r2 = Ok(vec![ReadResponse::from_request_with_data(&request, &[1])]);

        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ds.process_ds_completion(
                    id,
                    ClientId::new(1),
                    r2,
                    &UpstairsState::Active,
                    None,
                )
            }));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn work_read_hash_mismatch_ack() {
        // Test that a hash mismatch will trigger a panic.
        // We check here after a ACK, because that is a different location.
        let mut ds = Downstairs::test_default();

        let id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, id);

        ds.enqueue(op);

        ds.in_progress(id, ClientId::new(0));
        ds.in_progress(id, ClientId::new(1));

        // Generate the first read response, this will be what we compare
        // future responses with.
        let r1 = Ok(vec![ReadResponse::from_request_with_data(&request, &[0])]);

        ds.process_ds_completion(
            id,
            ClientId::new(0),
            r1,
            &UpstairsState::Active,
            None,
        );

        // We must move the completed job along the process, this enables
        // process_ds_completion to know to compare future jobs to this
        // one.
        ds.ack(id);

        // Second read response, it matches the first.
        let r2 = Ok(vec![ReadResponse::from_request_with_data(&request, &[1])]);

        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ds.process_ds_completion(
                    id,
                    ClientId::new(1),
                    r2,
                    &UpstairsState::Active,
                    None,
                )
            }));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn work_read_hash_mismatch_third() {
        // Test that a hash mismatch on the third response will trigger a panic.
        let mut ds = Downstairs::test_default();

        let id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, id);

        ds.enqueue(op);

        ds.in_progress(id, ClientId::new(0));
        ds.in_progress(id, ClientId::new(1));
        ds.in_progress(id, ClientId::new(2));

        // Generate the first read response, this will be what we compare
        // future responses with.
        let r1 = Ok(vec![ReadResponse::from_request_with_data(&request, &[1])]);

        ds.process_ds_completion(
            id,
            ClientId::new(0),
            r1,
            &UpstairsState::Active,
            None,
        );

        // Second read response, it matches the first.
        let r2 = Ok(vec![ReadResponse::from_request_with_data(&request, &[1])]);

        ds.process_ds_completion(
            id,
            ClientId::new(1),
            r2,
            &UpstairsState::Active,
            None,
        );

        let r3 = Ok(vec![ReadResponse::from_request_with_data(&request, &[2])]);

        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ds.process_ds_completion(
                    id,
                    ClientId::new(2),
                    r3,
                    &UpstairsState::Active,
                    None,
                )
            }));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn work_read_hash_mismatch_third_ack() {
        // Test that a hash mismatch on the third response will trigger a panic.
        // This one checks after an ACK.
        let mut ds = Downstairs::test_default();

        let id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, id);

        ds.enqueue(op);

        ds.in_progress(id, ClientId::new(0));
        ds.in_progress(id, ClientId::new(1));
        ds.in_progress(id, ClientId::new(2));

        // Generate the first read response, this will be what we compare
        // future responses with.
        let r1 = Ok(vec![ReadResponse::from_request_with_data(&request, &[1])]);

        ds.process_ds_completion(
            id,
            ClientId::new(0),
            r1,
            &UpstairsState::Active,
            None,
        );

        // Second read response, it matches the first.
        let r2 = Ok(vec![ReadResponse::from_request_with_data(&request, &[1])]);

        ds.ack(id);
        ds.process_ds_completion(
            id,
            ClientId::new(1),
            r2,
            &UpstairsState::Active,
            None,
        );

        let r3 = Ok(vec![ReadResponse::from_request_with_data(&request, &[2])]);

        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ds.process_ds_completion(
                    id,
                    ClientId::new(2),
                    r3,
                    &UpstairsState::Active,
                    None,
                )
            }));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn work_read_hash_mismatch_inside() {
        // Test that a hash length mismatch will panic
        let mut ds = Downstairs::test_default();

        let id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, id);

        ds.enqueue(op);

        ds.in_progress(id, ClientId::new(0));
        ds.in_progress(id, ClientId::new(1));

        // Generate the first read response, this will be what we compare
        // future responses with.
        let r1 = Ok(vec![ReadResponse::from_request_with_data(
            &request,
            &[1, 2, 3, 4],
        )]);

        ds.process_ds_completion(
            id,
            ClientId::new(0),
            r1,
            &UpstairsState::Active,
            None,
        );

        // Second read response, hash vec has different length/
        let r2 = Ok(vec![ReadResponse::from_request_with_data(
            &request,
            &[1, 2, 3, 9],
        )]);

        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ds.process_ds_completion(
                    id,
                    ClientId::new(1),
                    r2,
                    &UpstairsState::Active,
                    None,
                )
            }));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn work_read_hash_mismatch_no_data() {
        // Test that empty data first, then data later will trigger
        // hash mismatch panic.
        let mut ds = Downstairs::test_default();

        let id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, id);

        ds.enqueue(op);

        ds.in_progress(id, ClientId::new(0));
        ds.in_progress(id, ClientId::new(1));

        // Generate the first read response, this will be what we compare
        // future responses with.
        let r1 = Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);

        ds.process_ds_completion(
            id,
            ClientId::new(0),
            r1,
            &UpstairsState::Active,
            None,
        );

        // Second read response, hash vec has different length/
        let r2 = Ok(vec![ReadResponse::from_request_with_data(&request, &[1])]);

        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ds.process_ds_completion(
                    id,
                    ClientId::new(1),
                    r2,
                    &UpstairsState::Active,
                    None,
                )
            }));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn work_read_hash_mismatch_no_data_next() {
        // Test that missing data on the 2nd read response will panic
        let mut ds = Downstairs::test_default();

        let id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, id);

        ds.enqueue(op);

        ds.in_progress(id, ClientId::new(0));
        ds.in_progress(id, ClientId::new(1));

        // Generate the first read response, this will be what we compare
        // future responses with.
        let r1 = Ok(vec![ReadResponse::from_request_with_data(&request, &[1])]);

        ds.process_ds_completion(
            id,
            ClientId::new(0),
            r1,
            &UpstairsState::Active,
            None,
        );

        // Second read response, hash vec has different length/
        let r2 = Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);

        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ds.process_ds_completion(
                    id,
                    ClientId::new(1),
                    r2,
                    &UpstairsState::Active,
                    None,
                )
            }));
        assert!(result.is_err());
    }

    #[test]
    fn work_write_unwritten_errors_are_counted() {
        // Verify that write IO errors are counted.
        work_errors_are_counted(false);
    }

    // Verify that write_unwritten IO errors are counted.
    #[test]
    fn work_write_errors_are_counted() {
        work_errors_are_counted(true);
    }

    // Instead of copying all the write tests, we put a wrapper around them
    // that takes write_unwritten as an arg.
    fn work_errors_are_counted(is_write_unwritten: bool) {
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();

        // send a write, and clients 0 and 1 will return errors

        let (request, iblocks) = generic_write_request();
        let op = ds.create_write_eob(
            next_id,
            iblocks,
            10,
            vec![request],
            is_write_unwritten,
        );

        ds.enqueue(op);

        assert!(ds.in_progress(next_id, ClientId::new(0)).is_some());
        assert!(ds.in_progress(next_id, ClientId::new(1)).is_some());
        assert!(ds.in_progress(next_id, ClientId::new(2)).is_some());

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));

        assert!(ds.ds_active.get(&next_id).unwrap().data.is_none());

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));

        assert!(ds.ds_active.get(&next_id).unwrap().data.is_none());

        // Before the last response, the write is marked as ackable, but
        // write_unwritten is not.
        assert_eq!(ds.ackable_work.len(), !is_write_unwritten as usize);

        let response = Ok(vec![]);
        let res = ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            response,
            &UpstairsState::Active,
            None,
        );

        // If it's write_unwritten, then this should have returned true,
        // if it's just a write, then it should be false.
        assert_eq!(res, is_write_unwritten);

        // After the last response, both write and write_unwritten should be
        // marked as ackable.
        assert_eq!(ds.ackable_work.len(), 1);

        assert!(ds.clients[ClientId::new(0)].stats.downstairs_errors > 0);
        assert!(ds.clients[ClientId::new(1)].stats.downstairs_errors > 0);
        assert_eq!(ds.clients[ClientId::new(2)].stats.downstairs_errors, 0);
    }

    #[test]
    fn work_delay_completion_flush_write() {
        work_delay_completion_flush(false);
    }

    #[test]
    fn work_delay_completion_flush_write_unwritten() {
        work_delay_completion_flush(true);
    }

    fn work_delay_completion_flush(is_write_unwritten: bool) {
        // Verify that a write/write_unwritten remains on the active
        // queue until a flush comes through and clears it.  In this case,
        // we only complete 2/3 for each IO.  We later come back and finish
        // the 3rd IO and the flush, which then allows the work to be
        // completed.
        let mut ds = Downstairs::test_default();

        // Create two writes, put them on the work queue
        let id1 = ds.next_id();
        let id2 = ds.next_id();

        let (request, iblocks) = generic_write_request();
        let op = ds.create_write_eob(
            id1,
            iblocks,
            10,
            vec![request],
            is_write_unwritten,
        );
        ds.enqueue(op);

        let (request, iblocks) = generic_write_request();
        let op = ds.create_write_eob(
            id2,
            iblocks,
            20,
            vec![request],
            is_write_unwritten,
        );
        ds.enqueue(op);

        // Simulate sending both writes to downstairs 0 and 1
        assert!(ds.in_progress(id1, ClientId::new(0)).is_some());
        assert!(ds.in_progress(id1, ClientId::new(1)).is_some());
        assert!(ds.in_progress(id2, ClientId::new(0)).is_some());
        assert!(ds.in_progress(id2, ClientId::new(1)).is_some());

        // Simulate completing both writes to downstairs 0 and 1
        //
        // write_unwritten jobs become ackable upon the second completion;
        // normal writes were ackable from the start (and hence
        // process_ds_completion always returns `false`)
        assert!(!ds.process_ds_completion(
            id1,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));
        assert_eq!(
            ds.process_ds_completion(
                id1,
                ClientId::new(1),
                Ok(vec![]),
                &UpstairsState::Active,
                None,
            ),
            is_write_unwritten
        );
        assert!(!ds.process_ds_completion(
            id2,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));
        assert_eq!(
            ds.process_ds_completion(
                id2,
                ClientId::new(1),
                Ok(vec![]),
                &UpstairsState::Active,
                None,
            ),
            is_write_unwritten
        );

        // Both writes can now ACK to the guest.
        ds.ack(id1);
        ds.ack(id2);

        // Work stays on active queue till the flush
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        // Create the flush, put on the work queue
        let flush_id = ds.next_id();
        let dep = ds.ds_active.deps_for_flush(flush_id);
        let op = Downstairs::create_flush(flush_id, dep, 10, 0, 0, None, None);
        ds.enqueue(op);

        // Simulate sending the flush to downstairs 0 and 1
        ds.in_progress(flush_id, ClientId::new(0));
        ds.in_progress(flush_id, ClientId::new(1));

        // Simulate completing the flush to downstairs 0 and 1
        assert!(!ds.process_ds_completion(
            flush_id,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));
        assert!(ds.process_ds_completion(
            flush_id,
            ClientId::new(1),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));

        // Ack the flush back to the guest
        ds.ack(flush_id);

        // Make sure downstairs 0 and 1 update their last flush id and
        // that downstairs 2 does not.
        assert_eq!(ds.clients[ClientId::new(0)].last_flush, flush_id);
        assert_eq!(ds.clients[ClientId::new(1)].last_flush, flush_id);
        assert_eq!(ds.clients[ClientId::new(2)].last_flush, JobId(0));

        // Should not retire yet.
        ds.retire_check(flush_id);

        assert!(ds.ackable_work.is_empty());

        // Make sure all work is still on the active side
        assert!(ds.completed.is_empty());

        // Now, finish the writes to downstairs 2
        assert!(ds.in_progress(id1, ClientId::new(2)).is_some());
        assert!(ds.in_progress(id2, ClientId::new(2)).is_some());
        assert!(!ds.process_ds_completion(
            id1,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));
        assert!(!ds.process_ds_completion(
            id2,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));

        // The job should not move to completed until the flush goes as well.
        assert!(ds.completed.is_empty());

        // Complete the flush on downstairs 2.
        ds.in_progress(flush_id, ClientId::new(2));
        assert!(!ds.process_ds_completion(
            flush_id,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));

        // All three jobs should now move to completed
        assert_eq!(ds.completed.len(), 3);
        // Downstairs 2 should update the last flush it just did.
        assert_eq!(ds.clients[ClientId::new(2)].last_flush, flush_id);
    }

    #[tokio::test]
    async fn work_assert_reads_do_not_cause_failure_state_transition() {
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();

        // send a read, and clients 0 and 1 will return errors

        let (request, op) = create_generic_read_eob(&mut ds, next_id);

        ds.enqueue(op);

        assert!(ds.in_progress(next_id, ClientId::new(0)).is_some());
        assert!(ds.in_progress(next_id, ClientId::new(1)).is_some());
        assert!(ds.in_progress(next_id, ClientId::new(2)).is_some());

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));

        assert!(ds.ds_active.get(&next_id).unwrap().data.is_none());

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));

        assert!(ds.ds_active.get(&next_id).unwrap().data.is_none());

        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[3])]);

        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            response,
            &UpstairsState::Active,
            None
        ));

        let responses = ds.ds_active.get(&next_id).unwrap().data.as_ref();
        assert!(responses.is_some());
        assert_eq!(
            responses.map(|responses| responses
                .iter()
                .map(|response| response.data.clone().freeze())
                .collect()),
            Some(vec![Bytes::from_static(&[3])]),
        );

        assert_eq!(ds.clients[ClientId::new(0)].stats.downstairs_errors, 0);
        assert_eq!(ds.clients[ClientId::new(1)].stats.downstairs_errors, 0);
        assert_eq!(ds.clients[ClientId::new(2)].stats.downstairs_errors, 0);

        // send another read, and expect all to return something
        // (reads shouldn't cause a Failed transition)

        let next_id = ds.next_id();
        let (request, op) = create_generic_read_eob(&mut ds, next_id);

        ds.enqueue(op);

        assert!(ds.in_progress(next_id, ClientId::new(0)).is_some());
        assert!(ds.in_progress(next_id, ClientId::new(1)).is_some());
        assert!(ds.in_progress(next_id, ClientId::new(2)).is_some());

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));

        assert!(ds.ds_active.get(&next_id).unwrap().data.is_none());

        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Err(CrucibleError::GenericError("bad".to_string())),
            &UpstairsState::Active,
            None,
        ));

        assert!(ds.ds_active.get(&next_id).unwrap().data.is_none());

        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[6])]);

        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            response,
            &UpstairsState::Active,
            None
        ));

        let responses = ds.ds_active.get(&next_id).unwrap().data.as_ref();
        assert!(responses.is_some());
        assert_eq!(
            responses.map(|responses| responses
                .iter()
                .map(|response| response.data.clone().freeze())
                .collect()),
            Some(vec![Bytes::from_static(&[6])]),
        );
    }

    #[tokio::test]
    async fn work_completed_read_flush() {
        // Verify that a read remains on the active queue until a flush
        // comes through and clears it.
        let mut ds = Downstairs::test_default();

        // Build our read, put it into the work queue
        let next_id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, next_id);

        ds.enqueue(op);

        // Move the work to submitted like we sent it to each downstairs
        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        // Downstairs 0 now has completed this work.
        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);
        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None
        ));

        // One completion of a read means we can ACK
        assert_eq!(ds.ackable_work.len(), 1);

        // Complete downstairs 1 and 2
        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            response,
            &UpstairsState::Active,
            None
        ));

        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            response,
            &UpstairsState::Active,
            None
        ));

        // Make sure the job is still active
        assert!(ds.completed.is_empty());

        // Ack the job to the guest (this checks whether it's ack ready)
        ds.ack(next_id);

        // Nothing left to ACK, but until the flush we keep the IO data.
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        // A flush is required to move work to completed
        // Create the flush then send it to all downstairs.
        let next_id = ds.next_id();
        let deps = ds.ds_active.deps_for_flush(next_id);
        let op = Downstairs::create_flush(next_id, deps, 10, 0, 0, None, None);

        ds.enqueue(op);

        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        // Complete the Flush at each downstairs.
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None,
        ));
        // Two completed means we return true (ack ready now)
        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));

        // ACK the flush and let retire_check move things along.
        ds.ack(next_id);
        ds.retire_check(next_id);

        // Verify no more work to ack.
        assert!(ds.ackable_work.is_empty());
        // The read and the flush should now be moved to completed.
        assert_eq!(ds.completed.len(), 2);
    }

    #[tokio::test]
    async fn retire_dont_retire_everything() {
        // Verify that a read not ACKED remains on the active queue even
        // if a flush comes through after it.
        let mut ds = Downstairs::test_default();

        // Build our read, put it into the work queue
        let next_id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, next_id);

        ds.enqueue(op);

        // Move the work to submitted like we sent it to each downstairs
        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        // Downstairs 0 now has completed this work.
        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);
        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None
        ));

        // One completion of a read means we can ACK
        assert_eq!(ds.ackable_work.len(), 1);

        // But, don't send the ack just yet.
        // The job should be ack ready
        assert!(ds.ackable_work.contains(&next_id));
        assert!(!ds.ds_active.get(&next_id).unwrap().acked);

        // A flush is required to move work to completed
        // Create the flush then send it to all downstairs.
        let next_id = ds.next_id();
        let deps = ds.ds_active.deps_for_flush(next_id);
        let op = Downstairs::create_flush(next_id, deps, 10, 0, 0, None, None);

        ds.enqueue(op);

        // Send and complete the Flush at each downstairs.
        for cid in ClientId::iter() {
            ds.in_progress(next_id, cid);
            ds.process_ds_completion(
                next_id,
                cid,
                Ok(vec![]),
                &UpstairsState::Active,
                None,
            );
        }

        // ACK the flush and let retire_check move things along.
        ds.ack(next_id);
        ds.retire_check(next_id);

        // Verify the read is still ack ready.
        assert_eq!(ds.ackable_work.len(), 1);
        // The the flush should now be moved to completed.
        assert_eq!(ds.completed.len(), 1);
        // The read should still be on the queue.
        assert_eq!(ds.ds_active.len(), 1);
    }

    #[test]
    fn work_completed_write_flush() {
        work_completed_writeio_flush(false);
    }

    #[test]
    fn work_completed_write_unwritten_flush() {
        work_completed_writeio_flush(true);
    }

    fn work_completed_writeio_flush(is_write_unwritten: bool) {
        // Verify that a write or write_unwritten remains on the active
        // queue until a flush comes through and clears it.
        let mut ds = Downstairs::test_default();

        // Build our write IO.
        let next_id = ds.next_id();

        let (request, iblocks) = generic_write_request();
        let op = ds.create_write_eob(
            next_id,
            iblocks,
            10,
            vec![request],
            is_write_unwritten,
        );
        // Put the write on the queue.
        ds.enqueue(op);

        // Submit the write to all three downstairs.
        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        // Complete the write on all three downstairs.
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
        assert_eq!(
            ds.process_ds_completion(
                next_id,
                ClientId::new(1),
                Ok(vec![]),
                &UpstairsState::Active,
                None
            ),
            is_write_unwritten
        );
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));

        // Ack the write to the guest
        ds.ack(next_id);

        // Work stays on active queue till the flush
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        // Create the flush IO
        let next_id = ds.next_id();
        let dep = ds.ds_active.deps_for_flush(next_id);
        let op = Downstairs::create_flush(next_id, dep, 10, 0, 0, None, None);
        ds.enqueue(op);

        // Submit the flush to all three downstairs.
        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        // Complete the flush on all three downstairs.
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));

        ds.ack(next_id);
        ds.retire_check(next_id);

        assert!(ds.ackable_work.is_empty());
        // The write and flush should now be completed.
        assert_eq!(ds.completed.len(), 2);
    }

    #[test]
    fn work_delay_completion_flush_order_write() {
        work_delay_completion_flush_order(false);
    }

    #[test]
    fn work_delay_completion_flush_order_write_unwritten() {
        work_delay_completion_flush_order(true);
    }

    fn work_delay_completion_flush_order(is_write_unwritten: bool) {
        // Verify that a write/write_unwritten remains on the active queue
        // until a flush comes through and clears it.  In this case, we only
        // complete 2 of 3 for each IO.  We later come back and finish the
        // 3rd IO and the flush, which then allows the work to be completed.
        // Also, we mix up which client finishes which job first.
        let mut ds = Downstairs::test_default();

        // Build two writes, put them on the work queue.
        let id1 = ds.next_id();
        let id2 = ds.next_id();

        let (request, iblocks) = generic_write_request();
        let op = ds.create_write_eob(
            id1,
            iblocks,
            10,
            vec![request],
            is_write_unwritten,
        );
        ds.enqueue(op);

        let (request, iblocks) = generic_write_request();
        let op = ds.create_write_eob(
            id2,
            iblocks,
            1,
            vec![request],
            is_write_unwritten,
        );
        ds.enqueue(op);

        // Submit the two writes, to 2/3 of the downstairs.
        assert!(ds.in_progress(id1, ClientId::new(0)).is_some());
        assert!(ds.in_progress(id1, ClientId::new(1)).is_some());
        assert!(ds.in_progress(id2, ClientId::new(1)).is_some());
        assert!(ds.in_progress(id2, ClientId::new(2)).is_some());

        // Complete the writes that we sent to the 2 downstairs.
        assert!(!ds.process_ds_completion(
            id1,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
        assert_eq!(
            ds.process_ds_completion(
                id1,
                ClientId::new(1),
                Ok(vec![]),
                &UpstairsState::Active,
                None
            ),
            is_write_unwritten
        );
        assert!(!ds.process_ds_completion(
            id2,
            ClientId::new(1),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
        assert_eq!(
            ds.process_ds_completion(
                id2,
                ClientId::new(2),
                Ok(vec![]),
                &UpstairsState::Active,
                None
            ),
            is_write_unwritten
        );

        // Ack the writes to the guest.
        ds.ack(id1);
        ds.ack(id2);

        // Work stays on active queue till the flush.
        assert!(ds.ackable_work.is_empty());
        assert!(ds.completed.is_empty());

        // Create and enqueue the flush.
        let flush_id = ds.next_id();
        let dep = ds.ds_active.deps_for_flush(flush_id);
        let op = Downstairs::create_flush(flush_id, dep, 10, 0, 0, None, None);
        ds.enqueue(op);

        // Send the flush to two downstairs.
        ds.in_progress(flush_id, ClientId::new(0));
        ds.in_progress(flush_id, ClientId::new(2));

        // Complete the flush on those downstairs.
        assert!(!ds.process_ds_completion(
            flush_id,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
        assert!(ds.process_ds_completion(
            flush_id,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));

        // Ack the flush
        ds.ack(flush_id);

        // Should not retire yet
        ds.retire_check(flush_id);

        assert!(ds.ackable_work.is_empty());
        // Not done yet, until all clients do the work.
        assert!(ds.completed.is_empty());

        // Verify who has updated their last flush.
        assert_eq!(ds.clients[ClientId::new(0)].last_flush, flush_id);
        assert_eq!(ds.clients[ClientId::new(1)].last_flush, JobId(0));
        assert_eq!(ds.clients[ClientId::new(2)].last_flush, flush_id);

        // Now, finish sending and completing the writes
        assert!(ds.in_progress(id1, ClientId::new(2)).is_some());
        assert!(ds.in_progress(id2, ClientId::new(0)).is_some());
        assert!(!ds.process_ds_completion(
            id1,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
        assert!(!ds.process_ds_completion(
            id2,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));

        // Completed work won't happen till the last flush is done
        assert!(ds.completed.is_empty());

        // Send and complete the flush
        ds.in_progress(flush_id, ClientId::new(1));
        assert!(!ds.process_ds_completion(
            flush_id,
            ClientId::new(1),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));

        // Now, all three jobs (w,w,f) will move to completed.
        assert_eq!(ds.completed.len(), 3);

        // downstairs 1 should now have that flush
        assert_eq!(ds.clients[ClientId::new(1)].last_flush, flush_id);
    }

    #[tokio::test]
    async fn work_completed_read_replay() {
        // Verify that a single read will replay
        let mut ds = Downstairs::test_default();

        // Build our read IO and submit it to the work queue.
        let next_id = ds.next_id();
        let (request, op) = create_generic_read_eob(&mut ds, next_id);
        ds.enqueue(op);

        // Submit the read to all three downstairs
        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        // Complete the read on one downstairs.
        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);
        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None
        ));

        // One completion should allow for an ACK
        assert_eq!(ds.ackable_work.len(), 1);
        assert!(ds.ackable_work.contains(&next_id));
        assert!(!ds.ds_active.get(&next_id).unwrap().acked);

        // The Downstairs will ack jobs immediately, during the event handler
        ds.ack(next_id);

        // Be sure the job is not yet in replay
        assert!(!ds.ds_active.get(&next_id).unwrap().replay);
        ds.replay_jobs(ClientId::new(0));
        // Now the IO should be replay
        assert!(ds.ds_active.get(&next_id).unwrap().replay);

        // The IO is still acked
        assert!(!ds.ackable_work.contains(&next_id));
        assert!(ds.ds_active.get(&next_id).unwrap().acked);
    }

    #[tokio::test]
    async fn work_completed_two_read_replay() {
        // Verify that a read will replay and acks are handled correctly if
        // there is more than one done read.
        let mut ds = Downstairs::test_default();

        // Build a read and put it on the work queue.
        let next_id = ds.next_id();
        let (request, op) = create_generic_read_eob(&mut ds, next_id);
        ds.enqueue(op);

        // Submit the read to each downstairs.
        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        // Complete the read on one downstairs, verify it is ack ready.
        let response = Ok(vec![ReadResponse::from_request_with_data(
            &request,
            &[1, 2, 3, 4],
        )]);
        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None
        ));
        // We always ack jobs right away
        ds.ack(next_id);

        // Complete the read on a 2nd downstairs.
        let response = Ok(vec![ReadResponse::from_request_with_data(
            &request,
            &[1, 2, 3, 4],
        )]);
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            response,
            &UpstairsState::Active,
            None
        ));

        // Now, take the first downstairs offline.
        ds.replay_jobs(ClientId::new(0));

        // The ack should still be done
        assert!(ds.ds_active.get(&next_id).unwrap().acked);

        // Taking the second downstairs offline, the ack should still be done
        ds.replay_jobs(ClientId::new(1));
        assert!(ds.ds_active.get(&next_id).unwrap().acked);

        // Redo the read on DS 0, IO should remain acked
        ds.in_progress(next_id, ClientId::new(0));

        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.ds_active.get(&next_id).unwrap().acked);
    }

    #[tokio::test]
    async fn work_completed_ack_read_replay() {
        // Verify that a read we Acked will still replay if that downstairs
        // goes away. Make sure everything still finishes ok.
        let mut ds = Downstairs::test_default();

        // Create the read and put it on the work queue.
        let next_id = ds.next_id();
        let (request, op) = create_generic_read_eob(&mut ds, next_id);
        ds.enqueue(op);

        // Submit the read to each downstairs.
        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        // Complete the read on one downstairs.
        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);
        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None
        ));
        // Immediately ack the job (which checks that it's ready)
        ds.ack(next_id);

        // Should not retire yet
        ds.retire_check(next_id);

        // No new ackable work.
        assert!(ds.ackable_work.is_empty());
        // Verify the IO has not completed yet.
        assert!(ds.completed.is_empty());

        // Now, take that downstairs offline
        ds.replay_jobs(ClientId::new(0));

        // Acked IO should remain so.
        assert!(ds.ds_active.get(&next_id).unwrap().acked);

        // Redo on DS 0, IO should remain acked.
        ds.in_progress(next_id, ClientId::new(0));
        let response =
            Ok(vec![ReadResponse::from_request_with_data(&request, &[])]);
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None
        ));
        assert!(ds.ackable_work.is_empty());
        assert!(ds.ds_active.get(&next_id).unwrap().acked);
    }

    // XXX another test that does read replay for the first IO
    // Also, the third IO?
    #[tokio::test]
    async fn work_completed_ack_read_replay_hash_mismatch() {
        // Verify that a read replay won't cause a panic on hash mismatch.
        // During a replay, the same block may have been written after a read,
        // so the actual contents of the block are now different.  We can't
        // compare the read hash taken before a replay to one that happens
        // after.  A sample sequence is:
        //
        // Flush (all previous IO cleared, all blocks the same)
        // Read block 0
        // Write block 0
        // - Downstairs goes away here, causing a replay.
        //
        // In this case, the replay read will get the data again, but that
        // data came from the write, which is different than the original,
        // pre-replay read data (and hash).  In this case we can't compare
        // with the original hash that we stored for this read.
        //
        // For the test below, we don't actually need to do a write, we
        // can just change the "data" we fill the response with like we
        // received different data than the original read.
        let mut ds = Downstairs::test_default();

        // Create the read and put it on the work queue.
        let next_id = ds.next_id();
        let (request, op) = create_generic_read_eob(&mut ds, next_id);
        ds.enqueue(op);

        // Submit the read to each downstairs.
        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        // Construct our fake response
        let response = Ok(vec![ReadResponse::from_request_with_data(
            &request,
            &[122], // Original data.
        )]);

        // Complete the read on one downstairs.
        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None
        ));

        // Ack the read to the guest.
        ds.ack(next_id);

        // Before re replay_jobs, the IO is not replay
        assert!(!ds.ds_active.get(&next_id).unwrap().replay);
        // Now, take that downstairs offline
        ds.replay_jobs(ClientId::new(0));
        // Now the IO should be replay
        assert!(ds.ds_active.get(&next_id).unwrap().replay);

        // Move it to in-progress.
        ds.in_progress(next_id, ClientId::new(0));

        // Now, create a new response that has different data, and will
        // produce a different hash.
        let response = Ok(vec![ReadResponse::from_request_with_data(
            &request,
            &[123], // Different data than before
        )]);

        // Process the new read (with different data), make sure we don't
        // trigger the hash mismatch check.
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None
        ));

        // Some final checks.  The replay should behave in every other way
        // like a regular read.
        assert!(ds.ds_active.get(&next_id).unwrap().acked);
    }

    #[tokio::test]
    async fn work_completed_ack_read_replay_two_hash_mismatch() {
        // Verify that a read replay won't cause a panic on hash mismatch.
        // During a replay, the same block may have been written after a read,
        // so the actual contents of the block are now different.  We can't
        // compare the read hash taken before a replay to one that happens
        // after.  A sample sequence is:
        //
        // Flush (all previous IO cleared, all blocks the same)
        // Read block 0
        // Write block 0
        //
        // In this case, the replay read will get the data again, but that
        // data came from the write, which is different than the original,
        // pre-replay read data (and hash).  In this case we can't compare
        // with the original hash that we stored for this read.
        //
        // For the test below, we don't actually need to do a write, we
        // can just change the "data" we fill the response with like we
        // received different data than the original read.
        let mut ds = Downstairs::test_default();

        // Create the read and put it on the work queue.
        let next_id = ds.next_id();
        let (request, op) = create_generic_read_eob(&mut ds, next_id);
        ds.enqueue(op);

        // Submit the read to each downstairs.
        ds.in_progress(next_id, ClientId::new(0));
        ds.in_progress(next_id, ClientId::new(1));
        ds.in_progress(next_id, ClientId::new(2));

        // Construct our fake response
        let response = Ok(vec![ReadResponse::from_request_with_data(
            &request,
            &[122], // Original data.
        )]);

        // Complete the read on one downstairs.
        assert!(ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None
        ));

        // Construct our fake response for another downstairs.
        let response = Ok(vec![ReadResponse::from_request_with_data(
            &request,
            &[122], // Original data.
        )]);

        // Complete the read on the this downstairs as well
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            response,
            &UpstairsState::Active,
            None
        ));

        // Ack the read to the guest.
        ds.ack(next_id);

        // Now, take the second downstairs offline
        ds.replay_jobs(ClientId::new(1));
        // Now the IO should be replay
        assert!(ds.ds_active.get(&next_id).unwrap().replay);

        // Move it to in-progress.
        ds.in_progress(next_id, ClientId::new(1));

        // Now, create a new response that has different data, and will
        // produce a different hash.
        let response = Ok(vec![ReadResponse::from_request_with_data(
            &request,
            &[123], // Different data than before
        )]);

        // Process the new read (with different data), make sure we don't
        // trigger the hash mismatch check.
        assert!(!ds.process_ds_completion(
            next_id,
            ClientId::new(1),
            response,
            &UpstairsState::Active,
            None
        ));

        // Some final checks.  The replay should behave in every other way
        // like a regular read.
        assert!(ds.ds_active.get(&next_id).unwrap().acked);
    }

    #[test]
    fn work_completed_write_ack_ready_replay_write() {
        work_completed_write_ack_ready_replay(false);
    }

    #[test]
    fn work_completed_write_ack_ready_replay_write_unwritten() {
        work_completed_write_ack_ready_replay(true);
    }

    fn work_completed_write_ack_ready_replay(is_write_unwritten: bool) {
        // Verify that a replay when we have two completed writes or
        // write_unwritten will change state from AckReady back to NotAcked.
        // If we then redo the work, it should go back to AckReady.
        let mut ds = Downstairs::test_default();

        // Create the write and put it on the work queue.
        let id1 = ds.next_id();
        let (request, iblocks) = generic_write_request();
        let op = ds.create_write_eob(
            id1,
            iblocks,
            10,
            vec![request],
            is_write_unwritten,
        );
        ds.enqueue(op);

        // Submit the read to two downstairs.
        assert!(ds.in_progress(id1, ClientId::new(0)).is_some());
        assert!(ds.in_progress(id1, ClientId::new(1)).is_some());

        // Complete the write on two downstairs.
        assert!(!ds.process_ds_completion(
            id1,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
        assert_eq!(
            ds.process_ds_completion(
                id1,
                ClientId::new(1),
                Ok(vec![]),
                &UpstairsState::Active,
                None
            ),
            is_write_unwritten
        );

        // Ack this job immediately
        ds.ack(id1);

        /* Now, take that downstairs offline */
        // Before replay_jobs, the IO is not replay
        assert!(!ds.ds_active.get(&id1).unwrap().replay);
        ds.replay_jobs(ClientId::new(1));
        // Now the IO should be replay
        assert!(ds.ds_active.get(&id1).unwrap().replay);

        // The job should remain acked
        assert!(ds.ds_active.get(&id1).unwrap().acked);

        // Re-submit and complete the write
        assert!(ds.in_progress(id1, ClientId::new(1)).is_some());
        assert!(!ds.process_ds_completion(
            id1,
            ClientId::new(1),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));

        // State should remain acked and not ackable
        assert!(ds.ds_active.get(&id1).unwrap().acked);
        assert!(ds.ackable_work.is_empty())
    }

    #[test]
    fn work_completed_write_acked_replay_write() {
        work_completed_write_acked_replay(false);
    }

    #[test]
    fn work_completed_write_acked_replay_write_unwritten() {
        work_completed_write_acked_replay(true);
    }

    fn work_completed_write_acked_replay(is_write_unwritten: bool) {
        // Verify that a replay when we have acked a write or write_unwritten
        // will not undo that ack.
        let mut ds = Downstairs::test_default();

        // Create the write and put it on the work queue.
        let id1 = ds.next_id();
        let (request, iblocks) = generic_write_request();
        let op = ds.create_write_eob(
            id1,
            iblocks,
            10,
            vec![request],
            is_write_unwritten,
        );
        ds.enqueue(op);

        // Submit the write to two downstairs.
        assert!(ds.in_progress(id1, ClientId::new(0)).is_some());
        assert!(ds.in_progress(id1, ClientId::new(1)).is_some());

        // Complete the write on two downstairs.
        assert!(!ds.process_ds_completion(
            id1,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
        assert_eq!(
            ds.process_ds_completion(
                id1,
                ClientId::new(1),
                Ok(vec![]),
                &UpstairsState::Active,
                None
            ),
            is_write_unwritten
        );

        // Verify it is ackable..
        assert_eq!(ds.ackable_work.len(), 1);

        // Send the ACK to the guest
        ds.ack(id1);

        // Verify no more ackable work
        assert!(ds.ackable_work.is_empty());

        // Now, take that downstairs offline
        ds.replay_jobs(ClientId::new(0));

        // State should stay acked
        assert!(ds.ds_active.get(&id1).unwrap().acked);

        // Finish the write all the way out.
        assert!(ds.in_progress(id1, ClientId::new(0)).is_some());
        assert!(ds.in_progress(id1, ClientId::new(2)).is_some());

        assert!(!ds.process_ds_completion(
            id1,
            ClientId::new(0),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
        assert!(!ds.process_ds_completion(
            id1,
            ClientId::new(2),
            Ok(vec![]),
            &UpstairsState::Active,
            None
        ));
    }

    #[test]
    fn reconcile_rc_to_message() {
        // Convert an extent fix to the crucible repair messages that
        // are sent to the downstairs.  Verify that the resulting
        // messages are what we expect
        let mut ds = Downstairs::test_default();
        let r0 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 801);
        let r1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 802);
        let r2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 803);
        ds.clients[ClientId::new(0)].repair_addr = Some(r0);
        ds.clients[ClientId::new(1)].repair_addr = Some(r1);
        ds.clients[ClientId::new(2)].repair_addr = Some(r2);

        let repair_extent = 9;
        let mut rec_list = HashMap::new();
        let ef = ExtentFix {
            source: ClientId::new(0),
            dest: vec![ClientId::new(1), ClientId::new(2)],
        };
        rec_list.insert(repair_extent, ef);
        let max_flush = 22;
        let max_gen = 33;
        ds.convert_rc_to_messages(rec_list, max_flush, max_gen);

        // Walk the list and check for messages we expect to find
        assert_eq!(ds.reconcile_task_list.len(), 4);

        // First task, flush
        let rio = ds.reconcile_task_list.pop_front().unwrap();
        assert_eq!(rio.id, ReconciliationId(0));
        match rio.op {
            Message::ExtentFlush {
                repair_id,
                extent_id,
                client_id,
                flush_number,
                gen_number,
            } => {
                assert_eq!(repair_id, ReconciliationId(0));
                assert_eq!(extent_id, repair_extent);
                assert_eq!(client_id, ClientId::new(0));
                assert_eq!(flush_number, max_flush);
                assert_eq!(gen_number, max_gen);
            }
            m => {
                panic!("{:?} not ExtentFlush()", m);
            }
        }
        assert_eq!(IOState::New, rio.state[ClientId::new(0)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(1)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(2)]);

        // Second task, close extent
        let rio = ds.reconcile_task_list.pop_front().unwrap();
        assert_eq!(rio.id, ReconciliationId(1));
        match rio.op {
            Message::ExtentClose {
                repair_id,
                extent_id,
            } => {
                assert_eq!(repair_id, ReconciliationId(1));
                assert_eq!(extent_id, repair_extent);
            }
            m => {
                panic!("{:?} not ExtentClose()", m);
            }
        }
        assert_eq!(IOState::New, rio.state[ClientId::new(0)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(1)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(2)]);

        // Third task, repair extent
        let rio = ds.reconcile_task_list.pop_front().unwrap();
        assert_eq!(rio.id, ReconciliationId(2));
        match rio.op {
            Message::ExtentRepair {
                repair_id,
                extent_id,
                source_client_id,
                source_repair_address,
                dest_clients,
            } => {
                assert_eq!(repair_id, rio.id);
                assert_eq!(extent_id, repair_extent);
                assert_eq!(source_client_id, ClientId::new(0));
                assert_eq!(source_repair_address, r0);
                assert_eq!(
                    dest_clients,
                    vec![ClientId::new(1), ClientId::new(2)]
                );
            }
            m => {
                panic!("{:?} not ExtentRepair", m);
            }
        }
        assert_eq!(IOState::New, rio.state[ClientId::new(0)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(1)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(2)]);

        // Third task, close extent
        let rio = ds.reconcile_task_list.pop_front().unwrap();
        assert_eq!(rio.id, ReconciliationId(3));
        match rio.op {
            Message::ExtentReopen {
                repair_id,
                extent_id,
            } => {
                assert_eq!(repair_id, ReconciliationId(3));
                assert_eq!(extent_id, repair_extent);
            }
            m => {
                panic!("{:?} not ExtentClose()", m);
            }
        }
        assert_eq!(IOState::New, rio.state[ClientId::new(0)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(1)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(2)]);
    }

    #[test]
    fn reconcile_rc_to_message_two() {
        // Convert another extent fix to the crucible repair messages that
        // are sent to the downstairs.  Verify that the resulting
        // messages are what we expect
        let mut ds = Downstairs::test_default();
        let r0 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 801);
        let r1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 802);
        let r2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 803);
        ds.clients[ClientId::new(0)].repair_addr = Some(r0);
        ds.clients[ClientId::new(1)].repair_addr = Some(r1);
        ds.clients[ClientId::new(2)].repair_addr = Some(r2);

        let repair_extent = 5;
        let mut rec_list = HashMap::new();
        let ef = ExtentFix {
            source: ClientId::new(2),
            dest: vec![ClientId::new(0), ClientId::new(1)],
        };
        rec_list.insert(repair_extent, ef);
        let max_flush = 66;
        let max_gen = 77;
        ds.convert_rc_to_messages(rec_list, max_flush, max_gen);

        // Walk the list and check for messages we expect to find
        assert_eq!(ds.reconcile_task_list.len(), 4);

        // First task, flush
        let rio = ds.reconcile_task_list.pop_front().unwrap();
        assert_eq!(rio.id, ReconciliationId(0));
        match rio.op {
            Message::ExtentFlush {
                repair_id,
                extent_id,
                client_id,
                flush_number,
                gen_number,
            } => {
                assert_eq!(repair_id, ReconciliationId(0));
                assert_eq!(extent_id, repair_extent);
                assert_eq!(client_id, ClientId::new(2));
                assert_eq!(flush_number, max_flush);
                assert_eq!(gen_number, max_gen);
            }
            m => {
                panic!("{:?} not ExtentFlush()", m);
            }
        }
        assert_eq!(IOState::New, rio.state[ClientId::new(0)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(1)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(2)]);

        // Second task, close extent
        let rio = ds.reconcile_task_list.pop_front().unwrap();
        assert_eq!(rio.id, ReconciliationId(1));
        match rio.op {
            Message::ExtentClose {
                repair_id,
                extent_id,
            } => {
                assert_eq!(repair_id, ReconciliationId(1));
                assert_eq!(extent_id, repair_extent);
            }
            m => {
                panic!("{:?} not ExtentClose()", m);
            }
        }
        assert_eq!(IOState::New, rio.state[ClientId::new(0)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(1)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(2)]);

        // Third task, repair extent
        let rio = ds.reconcile_task_list.pop_front().unwrap();
        assert_eq!(rio.id, ReconciliationId(2));
        match rio.op {
            Message::ExtentRepair {
                repair_id,
                extent_id,
                source_client_id,
                source_repair_address,
                dest_clients,
            } => {
                assert_eq!(repair_id, rio.id);
                assert_eq!(extent_id, repair_extent);
                assert_eq!(source_client_id, ClientId::new(2));
                assert_eq!(source_repair_address, r2);
                assert_eq!(
                    dest_clients,
                    vec![ClientId::new(0), ClientId::new(1)]
                );
            }
            m => {
                panic!("{:?} not ExtentRepair", m);
            }
        }
        assert_eq!(IOState::New, rio.state[ClientId::new(0)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(1)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(2)]);

        // Third task, close extent
        let rio = ds.reconcile_task_list.pop_front().unwrap();
        assert_eq!(rio.id, ReconciliationId(3));
        match rio.op {
            Message::ExtentReopen {
                repair_id,
                extent_id,
            } => {
                assert_eq!(repair_id, ReconciliationId(3));
                assert_eq!(extent_id, repair_extent);
            }
            m => {
                panic!("{:?} not ExtentClose()", m);
            }
        }
        assert_eq!(IOState::New, rio.state[ClientId::new(0)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(1)]);
        assert_eq!(IOState::New, rio.state[ClientId::new(2)]);
    }

    #[test]
    fn bad_decryption_means_panic() {
        // Failure to decrypt means panic.
        // This result has a valid hash, but won't decrypt.
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, next_id);

        let context = Arc::new(EncryptionContext::new(
            vec![
                0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x0, 0x1, 0x2, 0x3,
                0x4, 0x5, 0x6, 0x7, 0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7,
                0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7,
            ],
            512,
        ));

        ds.enqueue(op);

        ds.in_progress(next_id, ClientId::new(0));

        // fake read response from downstairs that will fail decryption

        let mut data = Vec::from([1u8; 512]);

        let (nonce, tag, _) = context.encrypt_in_place(&mut data).unwrap();

        let nonce: [u8; 12] = nonce.into();
        let mut tag: [u8; 16] = tag.into();

        // alter tag
        if tag[3] == 0xFF {
            tag[3] = 0x00;
        } else {
            tag[3] = 0xFF;
        }

        // compute integrity hash after alteration above! It should still
        // validate
        let hash = integrity_hash(&[&nonce, &tag, &data]);

        let response = Ok(vec![ReadResponse {
            eid: request.eid,
            offset: request.offset,

            data: BytesMut::from(&data[..]),
            block_contexts: vec![BlockContext {
                encryption_context: Some(
                    crucible_protocol::EncryptionContext { nonce, tag },
                ),
                hash,
            }],
        }]);

        // Don't use `should_panic`, as the `unwrap` above could cause this test
        // to pass for the wrong reason.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ds.process_ds_completion(
                    next_id,
                    ClientId::new(0),
                    response,
                    &UpstairsState::Active,
                    None,
                );
            }));

        assert!(result.is_err());
    }

    #[test]
    #[should_panic]
    fn bad_read_hash_means_panic() {
        // Verify that a bad hash on a read will panic
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, next_id);

        ds.enqueue(op);
        ds.in_progress(next_id, ClientId::new(0));

        // fake read response from downstairs that will fail integrity hash
        // check

        let data = Vec::from([1u8; 512]);

        let response = Ok(vec![ReadResponse {
            eid: request.eid,
            offset: request.offset,

            data: BytesMut::from(&data[..]),
            block_contexts: vec![BlockContext {
                encryption_context: None,
                hash: 10000, // junk hash,
            }],
        }]);

        let _result = ds.process_ds_completion(
            next_id,
            ClientId::new(0),
            response,
            &UpstairsState::Active,
            None,
        );
    }

    #[test]
    fn bad_hash_on_encrypted_read_panic() {
        // Verify that a decryption failure on a read will panic.
        let mut ds = Downstairs::test_default();

        let next_id = ds.next_id();

        let (request, op) = create_generic_read_eob(&mut ds, next_id);

        let context = Arc::new(EncryptionContext::new(
            vec![
                0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x0, 0x1, 0x2, 0x3,
                0x4, 0x5, 0x6, 0x7, 0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7,
                0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7,
            ],
            512,
        ));

        ds.enqueue(op);

        ds.in_progress(next_id, ClientId::new(0));

        // fake read response from downstairs that will fail integrity hash
        // check
        let mut data = Vec::from([1u8; 512]);

        let (nonce, tag, _) = context.encrypt_in_place(&mut data).unwrap();

        let nonce = nonce.into();
        let tag = tag.into();

        let response = Ok(vec![ReadResponse {
            eid: request.eid,
            offset: request.offset,

            data: BytesMut::from(&data[..]),
            block_contexts: vec![BlockContext {
                encryption_context: Some(
                    crucible_protocol::EncryptionContext { nonce, tag },
                ),
                hash: 10000, // junk hash,
            }],
        }]);

        // Don't use `should_panic`, as the `unwrap` above could cause this test
        // to pass for the wrong reason.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ds.process_ds_completion(
                    next_id,
                    ClientId::new(0),
                    response,
                    &UpstairsState::Active,
                    None,
                );
            }));

        assert!(result.is_err());
    }
}
