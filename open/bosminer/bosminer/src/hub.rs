// Copyright (C) 2019  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

//! Top level builder for `job::Solver` and `work::Solver` intended to be used when instantiating
//! the full miner

use ii_logging::macros::*;

use crate::backend;
use crate::client;
use crate::error;
use crate::hal::{self, BackendConfig};
use crate::node;
use crate::work;

use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::stream::StreamExt;

use std::sync::{Arc, Weak};

/// Handle external events. Currently it is used only wor handling exhausted work from work engine.
/// It usually signals some serious problem in backend.
#[derive(Debug)]
struct EventHandler;

impl work::ExhaustedHandler for EventHandler {
    fn handle_exhausted(&self, _engine: work::DynEngine) {
        warn!("No more work available for current job!");
    }
}

/// Responsible for delivering work solution to the client from which the work has been generated
struct SolutionRouter {
    job_executor: Arc<client::JobExecutor>,
    solution_receiver: mpsc::UnboundedReceiver<work::Solution>,
}

impl SolutionRouter {
    fn new(
        job_executor: Arc<client::JobExecutor>,
        solution_receiver: mpsc::UnboundedReceiver<work::Solution>,
    ) -> Self {
        Self {
            job_executor,
            solution_receiver,
        }
    }

    async fn run(mut self) {
        while let Some(solution) = self.solution_receiver.next().await {
            // NOTE: all solutions targeting to removed clients are discarded
            if let Some(solution_sender) = self.job_executor.get_solution_sender(&solution).await {
                solution_sender
                    .unbounded_send(solution)
                    .expect("solution queue send failed");
            } else {
                warn!("Hub: solution has been discarded because client does not exist anymore");
            }
        }
    }
}

pub struct Core {
    pub backend_info: Option<hal::BackendInfo>,
    // NOTE: Weak reference must be released first!
    backend_registry: Weak<backend::Registry>,
    pub frontend: Arc<crate::Frontend>,
    job_executor: Arc<client::JobExecutor>,
    engine_receiver: work::EngineReceiver,
    solution_sender: mpsc::UnboundedSender<work::Solution>,
    solution_router: Mutex<Option<SolutionRouter>>,
    /// Registry of clients that are able to supply new jobs for mining
    client_manager: client::Manager,
}

/// Concentrates handles to all nodes associated with mining (backends, clients, work solvers)
impl Core {
    pub fn new(
        midstate_count: usize,
        backend_registry: &Arc<backend::Registry>,
        backend_info: Option<hal::BackendInfo>,
    ) -> Self {
        let frontend = Arc::new(crate::Frontend::new());

        let (engine_sender, engine_receiver) = work::engine_channel(EventHandler);
        let (solution_sender, solution_receiver) = mpsc::unbounded();

        let client_manager = client::Manager::new(midstate_count);
        let job_executor = Arc::new(client::JobExecutor::new(
            frontend.clone(),
            engine_sender,
            client_manager.clone(),
        ));

        Self {
            backend_info,
            backend_registry: Arc::downgrade(backend_registry),
            frontend,
            job_executor: job_executor.clone(),
            engine_receiver,
            solution_sender,
            solution_router: Mutex::new(Some(SolutionRouter::new(job_executor, solution_receiver))),
            client_manager,
        }
    }

    /// Builds a new backend for a specified `backend_config`.
    /// The resulting `hal::FrontendConfig` is then available for starting additional BOSminer
    /// components
    pub async fn build_backend<T: hal::Backend>(
        &self,
        mut backend_config: T::Config,
    ) -> error::Result<hal::FrontendConfig> {
        let work_solver_builder = work::SolverBuilder::new(
            self.frontend.clone(),
            self.backend_registry
                .upgrade()
                .expect("BUG: missing backend registry"),
            self.engine_receiver.clone(),
            self.solution_sender.clone(),
        );

        backend_config.set_client_manager(self.get_client_manager().clone());
        // call backend create to determine the preferred hierarchy
        match T::create(&mut backend_config) {
            // the generic tree hierarchy where the backend consists of multiple devices
            node::WorkSolverType::WorkHub(create) => {
                let work_hub = work_solver_builder.create_work_hub(create).await;
                // Initialization of backend hierarchy is done dynamically with provided work hub
                // which can be used for registration of another work hubs or work solvers. The
                // hierarchy has no limitation but is restricted only with tree structure.
                T::init_work_hub(backend_config, work_hub).await
            }
            // the simplest hierarchy where the backend is single device
            node::WorkSolverType::WorkSolver(create) => {
                let work_solver = work_solver_builder.create_work_solver(create).await;
                T::init_work_solver(backend_config, work_solver).await
            }
        }
    }

    #[inline]
    pub async fn get_root_hub(&self) -> Option<Arc<dyn node::WorkSolver>> {
        self.backend_registry
            .upgrade()?
            .lock_root_hub()
            .await
            .clone()
    }

    #[inline]
    pub async fn get_work_hubs(&self) -> Vec<Arc<dyn node::WorkSolver>> {
        if let Some(backend_registry) = self.backend_registry.upgrade() {
            backend_registry
                .lock_work_hubs()
                .await
                .iter()
                .cloned()
                .collect()
        } else {
            vec![]
        }
    }

    #[inline]
    pub async fn get_work_solvers(&self) -> Vec<Arc<dyn node::WorkSolver>> {
        if let Some(backend_registry) = self.backend_registry.upgrade() {
            backend_registry
                .lock_work_solvers()
                .await
                .iter()
                .cloned()
                .collect()
        } else {
            vec![]
        }
    }

    pub fn get_client_manager(&self) -> &client::Manager {
        &self.client_manager
    }

    pub async fn run(self: Arc<Self>) {
        let solution_router = self
            .solution_router
            .lock()
            .await
            .take()
            .expect("missing solution router");

        tokio::spawn(solution_router.run());
        self.job_executor.clone().run().await;
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::job;
    use crate::test_utils;
    use crate::Frontend;

    use std::sync::Arc;

    /// Create job solver for frontend (pool) and work solver builder for backend (as we expect a
    /// hierarchical structure in backends)
    fn build_solvers() -> (job::Solver, work::SolverBuilder<Frontend>) {
        let (engine_sender, engine_receiver) = work::engine_channel(EventHandler);
        let (solution_sender, solution_receiver) = mpsc::unbounded();
        let frontend = Arc::new(crate::Frontend::new());
        let _ = engine_sender.replace_engine_generator(Box::new(move |job| {
            Arc::new(work::engine::VersionRolling::new(job, 1))
        }));
        (
            job::Solver::new(Arc::new(engine_sender), solution_receiver),
            work::SolverBuilder::new(
                frontend,
                Arc::new(backend::Registry::new()),
                engine_receiver,
                solution_sender,
            ),
        )
    }

    /// This test verifies the whole lifecycle of a mining job, its transformation into work
    /// and also collection of the solution via solution receiver. No actual mining takes place
    /// in the test
    #[tokio::test]
    async fn test_solvers_connection() {
        let (mut job_solver, work_solver_builder) = build_solvers();

        let mut work_generator = None;
        let mut solution_sender = None;

        work_solver_builder
            .create_work_solver(|local_work_generator, local_solution_sender| {
                work_generator = Some(local_work_generator);
                solution_sender = Some(local_solution_sender);
                Arc::new(test_utils::TestWorkSolver::new())
            })
            .await;

        let mut work_generator = work_generator.unwrap();
        let solution_sender = solution_sender.unwrap();

        // default target is be set to difficulty 1 so all solution should pass
        for block in test_utils::TEST_BLOCKS.iter() {
            let job = Arc::new(*block);

            // send prepared testing block to job solver
            job_solver.job_sender.send(job);
            // work generator receives this job and prepares work from it
            let work = work_generator.generate().await.unwrap();
            // initial value for version rolling is 0 so midstate should match with expected one
            assert_eq!(block.midstate, work.midstates[0].state);
            // test block has automatic conversion into work solution
            solution_sender.send(block.into());
            // this solution should pass through job solver
            let solution = job_solver.solution_receiver.receive().await.unwrap();
            // check if the solution is equal to expected one
            assert_eq!(block.nonce, solution.nonce());
            let original_job: &test_utils::TestBlock = solution.job();
            // the job should also match with original one
            // job solver does not returns Arc so the comparison is done by its hashes
            assert_eq!(block.hash, original_job.hash);
        }

        // work generator still works even if all job solvers are dropped
        drop(job_solver);
        assert!(work_generator.generate().await.is_some());
    }
}
