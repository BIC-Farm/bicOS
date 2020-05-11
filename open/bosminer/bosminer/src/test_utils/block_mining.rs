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

//! The purpose of this test is to verify that the mining functionality of bosminer hasn't been impaired.
//! This test is deterministic - we know hardware can mine all the test blocks in `test_utils`,
//! and we want to verify that we receive correct solution for each block (which tests
//! that all work has been correctly defined and sent to hardware).

use ii_logging::macros::*;

use ii_bitcoin::HashTrait;

use crate::backend;
use crate::hal::{self, BackendConfig as _};
use crate::job::Bitcoin;
use crate::node;
use crate::test_utils;
use crate::work;

use std::time::{Duration, Instant};

use tokio::time::delay_for;

use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::stream::StreamExt;

use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug)]
struct ExhaustedWorkHandler {
    reschedule_sender: mpsc::UnboundedSender<work::DynEngine>,
}

impl ExhaustedWorkHandler {
    pub fn new(reschedule_sender: mpsc::UnboundedSender<work::DynEngine>) -> Self {
        Self { reschedule_sender }
    }
}

impl work::ExhaustedHandler for ExhaustedWorkHandler {
    fn handle_exhausted(&self, engine: work::DynEngine) {
        self.reschedule_sender
            .unbounded_send(engine)
            .expect("reschedule notify send failed");
    }
}

/// Problem is a "work recipe" for mining hardware that is to have a particular
/// solution in a particular midstate.
/// The `model_solution` is a "template" after which this work is modeled.
#[derive(Clone)]
struct Problem {
    model_solution: work::Solution,
    target_midstate: usize,
}

impl Problem {
    fn new(model_solution: work::Solution, target_midstate: usize) -> Self {
        Self {
            model_solution,
            target_midstate,
        }
    }

    /// Problem can be converted to MiningWork.
    ///
    /// The in-soluble midstates (other than the one specified in the problem)
    /// are created from the original solution by increasing/decreasing the version
    /// slightly. There's no guarantee these blocks have no solution.
    fn into_work(self, midstate_count: usize) -> work::Assignment {
        let job: &test_utils::TestBlock = self.model_solution.job();
        let time = job.time();
        let correct_version = job.version();
        let mut midstates = Vec::with_capacity(midstate_count);

        // prepare block chunk1 with all invariants
        let mut block_chunk1 = ii_bitcoin::BlockHeader {
            previous_hash: job.previous_hash().into_inner(),
            merkle_root: job.merkle_root().into_inner(),
            ..Default::default()
        };

        // generate all midstates from given range of indexes
        for index in 0..midstate_count {
            // use index for generation compatible header version
            let version = correct_version ^ (index as u32) ^ (self.target_midstate as u32);
            block_chunk1.version = version;
            midstates.push(work::Midstate {
                version,
                state: block_chunk1.midstate(),
            })
        }
        work::Assignment::new(Arc::new(*job), midstates, time)
    }
}

impl std::fmt::Debug for Problem {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            fmt,
            "{:?} target_midstate={}",
            &self.model_solution, self.target_midstate
        )
    }
}

/// `Solution` represents a valid solution from hardware in a given index.
#[derive(Clone)]
struct Solution {
    solution: work::Solution,
    midstate_idx: usize,
}

impl Solution {
    fn new(solution: work::Solution, midstate_idx: usize) -> Self {
        Self {
            solution,
            midstate_idx,
        }
    }
}

impl std::fmt::Debug for Solution {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(fmt, "{:?}", &self.solution)
    }
}

impl From<work::Solution> for Solution {
    fn from(solution: work::Solution) -> Self {
        let midstate_idx = solution.midstate_idx();
        Self::new(solution, midstate_idx)
    }
}

/// `SolutionKey` is measure by which we pair in problems and solutions
/// If two problems have equal SolutionKeys, they are considered identical.
/// For now we use block hash and midstate index in which the work was solved.
#[derive(PartialEq, Eq, Clone, Debug, Hash)]
struct SolutionKey {
    hash: ii_bitcoin::DHash,
    midstate_idx: usize,
}

impl SolutionKey {
    fn from_problem(p: Problem) -> Self {
        Self {
            hash: *p.model_solution.hash(),
            midstate_idx: p.target_midstate,
        }
    }

    fn from_solution(solution: Solution) -> Self {
        Self {
            hash: *solution.solution.hash(),
            midstate_idx: solution.midstate_idx,
        }
    }
}

/// `SolutionState` is state of solution in registry.
/// It can be either solved or not solved.
/// When we create a new `SolutionState` (from PRoblem) we attach a job to it so
/// that we can figure out what jobs were not solved.
#[derive(Clone, Debug)]
struct SolutionState {
    solved: bool,
    problem: Problem,
}

impl SolutionState {
    fn new(problem: Problem) -> Self {
        Self {
            solved: false,
            problem,
        }
    }
}

/// Registry holds problems and pairs them with solutions
#[derive(Clone, Debug)]
struct Registry {
    map: HashMap<SolutionKey, SolutionState>,
}

impl Registry {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Adds problem to registry.
    /// Returns true if this problem is unique.
    fn add_problem(&mut self, problem: Problem) -> bool {
        trace!("adding problem: {:?}", &problem);
        let key = SolutionKey::from_problem(problem.clone());
        if self.map.get(&key).is_some() {
            return false;
        }
        self.map.insert(key, SolutionState::new(problem));
        true
    }

    /// Adds solution to registry.
    fn add_solution(&mut self, solution: Solution) {
        match self
            .map
            .get_mut(&SolutionKey::from_solution(solution.clone()))
        {
            Some(state) => state.solved = true,
            None => warn!("no problem for {:?}", solution),
        }
    }

    /// Checks if all problems in registry were solved.
    /// Prints the ones that were not solved.
    fn check_everything_solved(&self, print_missing_solutions: bool) -> bool {
        let mut everything_solved = true;
        for (_solution_key, solution_state) in self.map.iter() {
            if !solution_state.solved {
                if print_missing_solutions {
                    error!("no solution for block {:?}", solution_state.problem);
                }
                everything_solved = false;
            }
        }
        everything_solved
    }
}

/// This builds the solver chain:
/// - build `engine_sender`/`engine_receiver` pair to send engines to `Solver`
/// - add channel to `engine_sender` that will notify us of engine being exhausted
/// - make a channel to get solutions back
/// - build a solver and connect everything to it
fn build_solvers() -> (
    work::EngineSender,
    mpsc::UnboundedReceiver<work::Solution>,
    mpsc::UnboundedReceiver<work::DynEngine>,
    work::SolverBuilder<crate::Frontend>,
) {
    let (reschedule_sender, reschedule_receiver) = mpsc::unbounded();
    let (engine_sender, engine_receiver) =
        work::engine_channel(ExhaustedWorkHandler::new(reschedule_sender));
    let (solution_queue_tx, solution_queue_rx) = mpsc::unbounded();
    (
        // Send engines here (preferably OneWork engines)
        engine_sender,
        // Receive solutions from this
        solution_queue_rx,
        // Receive exhausted engines here (once OneWorkEngine has been turned into MiningWork,
        // then you will be able to receive it here)
        reschedule_receiver,
        // This is a solver that you hand off to backend
        work::SolverBuilder::new(
            Arc::new(crate::Frontend::new()),
            Arc::new(backend::IgnoreHierarchy),
            engine_receiver,
            solution_queue_tx,
        ),
    )
}

async fn collect_solutions(
    mut solution_queue_rx: mpsc::UnboundedReceiver<work::Solution>,
    registry: Arc<Mutex<Registry>>,
) {
    while let Some(solution) = solution_queue_rx.next().await {
        let job: &test_utils::TestBlock = solution.job();
        info!(
            "received: was={:08x} got={:08x} ms={} hash={}",
            job.nonce,
            solution.nonce(),
            solution.midstate_idx(),
            solution.hash()
        );
        registry.lock().await.add_solution(solution.into());
    }
}

pub async fn run<T: hal::Backend>(mut backend_config: T::Config) {
    // this is a small miner core: we generate work, collect solutions, and we pair them together
    // we expect all (generated) problems to be solved
    // ii_async_compat::run_main_exits(async move {
    // read config
    let midstate_count = backend_config.midstate_count();

    // Create solver and channels to send/receive work
    let (engine_sender, solution_queue_rx, mut reschedule_receiver, work_solver_builder) =
        build_solvers();

    // create problem registry
    let registry = Arc::new(Mutex::new(Registry::new()));

    // start HW backend for selected target
    match T::create(&mut backend_config) {
        node::WorkSolverType::WorkHub(create) => {
            let work_hub = work_solver_builder.create_work_hub(create).await;
            T::init_work_hub(backend_config, work_hub).await.unwrap();
        }
        node::WorkSolverType::WorkSolver(create) => {
            let work_solver = work_solver_builder.create_work_solver(create).await;
            T::init_work_solver(backend_config, work_solver)
                .await
                .unwrap();
        }
    }

    // start task to collect solutions and put them to registry
    tokio::spawn(collect_solutions(solution_queue_rx, registry.clone()));

    // TODO: first work sent to miner is for some reason ignored
    // workaround: send two works
    engine_sender.broadcast_engine(Arc::new(test_utils::OneWorkEngine::new(
        Problem::new((&test_utils::TEST_BLOCKS[0]).into(), 0).into_work(midstate_count),
    )));

    // generate all blocks for all possible midstates
    for target_midstate in 0..midstate_count {
        for test_block in test_utils::TEST_BLOCKS.iter() {
            let problem = Problem {
                model_solution: test_block.into(),
                target_midstate,
            };
            let is_unique = registry.lock().await.add_problem(problem.clone());
            if !is_unique {
                panic!("duplicate problem");
            }
            // wait for the work (engine) to be sent out (exhausted)
            reschedule_receiver.next().await;
            engine_sender.broadcast_engine(Arc::new(test_utils::OneWorkEngine::new(
                problem.clone().into_work(midstate_count),
            )));
        }
    }

    // wait for hw to finish computation
    let timeout_started = Instant::now();
    while timeout_started.elapsed() < T::JOB_TIMEOUT {
        delay_for(Duration::from_secs(1)).await;

        if registry.lock().await.check_everything_solved(false) {
            break;
        }
    }

    // go through registry and check if everything was solved
    let registry = registry.lock().await;
    assert!(registry.check_everything_solved(true));
    // });
}

#[test]
fn test_registry() {
    let mut registry = Registry::new();
    let block1: work::Solution = (&test_utils::TEST_BLOCKS[0]).into();
    let block2: work::Solution = (&test_utils::TEST_BLOCKS[1]).into();

    // problem can be inserted only once
    assert!(registry.add_problem(Problem::new(block1.clone(), 2)));
    assert!(!registry.add_problem(Problem::new(block1.clone(), 2)));
    // nothing is solved yet
    assert!(!registry.check_everything_solved(false));
    // solve everything and check
    registry.add_solution(Solution::new(block1.clone(), 2));
    assert!(registry.check_everything_solved(false));

    // re-inserting problem doesn't unsolve it
    assert!(!registry.add_problem(Problem::new(block1.clone(), 2)));
    assert!(registry.check_everything_solved(false));

    // test multiple problems
    assert!(registry.add_problem(Problem::new(block1.clone(), 1)));
    assert!(!registry.add_problem(Problem::new(block1.clone(), 1)));
    assert!(registry.add_problem(Problem::new(block2.clone(), 3)));
    assert!(!registry.check_everything_solved(false));
    registry.add_solution(Solution::new(block2.clone(), 3));
    assert!(!registry.check_everything_solved(false));
    registry.add_solution(Solution::new(block1.clone(), 1));
    assert!(registry.check_everything_solved(false));
}
