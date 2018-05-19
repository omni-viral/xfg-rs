use chain::{resource::Id, schedule::Schedule, sync::SyncData};
use hal::{Backend, Device, queue::{QueueFamily, QueueFamilyId, RawCommandQueue, RawSubmission}};
use std::{borrow::Borrow, ops::AddAssign};

use smallvec::SmallVec;

use id::{BufferId, ImageId};
use node::{Node, wrap::{AnyNode, NodeBuilder}};

pub struct Graph<B: Backend, D, T> {
    nodes: Vec<Box<AnyNode<B, D, T>>>,
    schedule: Schedule<SyncData<usize, usize>>,
    semaphores: Vec<B::Semaphore>,
}

impl<B, D, T> Graph<B, D, T>
where
    B: Backend,
    D: Device<B>,
{
    /// Perform graph execution.
    /// Run every node of the graph and submit resulting command buffers to the queues.
    ///
    /// # Parameters
    ///
    /// `frame`     - frame index. This index must be less than `frames` specified in `GraphBuilder::build`
    ///               Caller must wait for all `fences` from last time this function was called with same `frame` index.
    ///
    /// `cqueueus`  - function to get `CommandQueue` by `QueueFamilyId` and index.
    ///               `Graph` guarantees that it will submit only command buffers
    ///               allocated from the command pool associated with specified `QueueFamilyId`.
    ///
    /// `device`    - `Device<B>` implementation. `B::Device` or wrapper.
    ///
    /// `aux`       - auxiliary data that `Node`s use.
    ///
    /// `fences`    - vector of fences that will be signaled after all commands are complete.
    ///               Fences that are attached to last submissions of every queue are reset.
    ///               This function may not use all fences. Unused fences are left in signalled state.
    ///               If this function needs more fences they will be allocated from `device` and pushed to this `Vec`.
    ///               So it's OK to start with empty `Vec`.
    pub fn run<'a, C>(
        &mut self,
        frame: usize,
        mut cqueues: C,
        device: &mut D,
        aux: &mut T,
        fences: &mut Vec<B::Fence>,
    ) where
        C: FnMut(QueueFamilyId, usize) -> &'a mut B::CommandQueue,
    {
        let mut fence_index = 0;

        let ref semaphores = self.semaphores;
        for family in self.schedule.iter() {
            for queue in family.iter() {
                let qid = queue.id();
                let cqueue = cqueues(qid.family(), qid.index());
                for (sid, submission) in queue.iter() {
                    let ref mut node = self.nodes[submission.pass().0];

                    assert!(
                        submission.sync().acquire.signal.is_empty()
                            && submission.sync().release.wait.is_empty()
                    );

                    let wait = submission
                        .sync()
                        .acquire
                        .wait
                        .iter()
                        .map(|wait| (&semaphores[*wait.semaphore()], wait.stage()))
                        .collect::<SmallVec<[_; 16]>>();
                    let signal = submission
                        .sync()
                        .release
                        .signal
                        .iter()
                        .map(|signal| &semaphores[*signal.semaphore()])
                        .collect::<SmallVec<[_; 16]>>();

                    let mut cbufs = SmallVec::new();
                    node.run(frame, device, aux, &mut cbufs);

                    let raw_submission = RawSubmission {
                        wait_semaphores: &wait,
                        signal_semaphores: &signal,
                        cmd_buffers: cbufs,
                    };

                    let fence = if sid.index() == queue.len() - 1 {
                        while fences.len() <= fence_index {
                            fences.push(device.create_fence(false));
                        }
                        fence_index += 1;
                        Some(&fences[fence_index - 1])
                    } else {
                        None
                    };

                    unsafe {
                        cqueue.submit_raw(raw_submission, fence);
                    }
                }
            }
        }
    }
}

pub struct GraphBuilder<B: Backend, D, T> {
    builders: Vec<NodeBuilder<B, D, T>>,
    gen_buffer_id: GenId<u32>,
    gen_image_id: GenId<u32>,
}

impl<B, D, T> GraphBuilder<B, D, T>
where
    B: Backend,
    D: Device<B>,
{
    /// Allocate new buffer id.
    pub fn new_buffer_id(&mut self) -> BufferId {
        BufferId(Id::new(self.gen_buffer_id.next()))
    }

    /// Allocate new image id.
    pub fn new_image_id(&mut self) -> ImageId {
        ImageId(Id::new(self.gen_image_id.next()))
    }

    /// Add node to the graph.
    pub fn add_node<N>(&mut self, node: NodeBuilder<B, D, T>)
    where
        N: Node<B, D, T>,
    {
        self.builders.push(node);
    }

    /// Build `Graph`.
    ///
    /// # Parameters
    ///
    /// `frames`        - maximum number of frames `Graph` will render simultaneously.
    ///
    /// `families`      - `Iterator` of `B::QueueFamily`s.
    ///
    /// `device`    - `Device<B>` implementation. `B::Device` or wrapper.
    ///
    /// `aux`       - auxiliary data that `Node`s use.
    pub fn build<F>(
        &self,
        frames: usize,
        families: F,
        device: &mut D,
        aux: &mut T,
    ) -> Graph<B, D, T>
    where
        F: IntoIterator,
        F::Item: Borrow<B::QueueFamily>,
    {
        use chain::{build, pass::Pass};

        let families = families.into_iter().collect::<Vec<_>>();
        let families = families.iter().map(Borrow::borrow).collect::<Vec<_>>();

        let mut semaphores = GenId::new();

        let passes: Vec<Pass> = self.builders
            .iter()
            .enumerate()
            .map(|(i, b)| b.chain(i, &families))
            .collect();
        let chains = build(
            passes,
            |qid| find_family::<B, _>(families.iter().cloned(), qid).max_queues(),
            || {
                let id = semaphores.next();
                (id, id)
            },
        );

        let mut nodes: Vec<Option<Box<AnyNode<B, D, T>>>> =
            (0..self.builders.len()).map(|_| None).collect();

        for family in chains.schedule.iter() {
            for queue in family.iter() {
                for (sid, submission) in queue.iter() {
                    let node = self.builders[submission.pass().0].build(
                        submission,
                        &chains.images,
                        frames,
                        find_family::<B, _>(families.iter().cloned(), sid.family()),
                        device,
                        aux,
                    );
                    nodes[submission.pass().0] = Some(node);
                }
            }
        }

        Graph {
            nodes: nodes.into_iter().map(Option::unwrap).collect(),
            schedule: chains.schedule,
            semaphores: (0..semaphores.total())
                .map(|_| device.create_semaphore())
                .collect(),
        }
    }
}

struct GenId<T> {
    next: T,
}

impl<T> GenId<T>
where
    T: Copy + From<u8> + AddAssign,
{
    fn new() -> Self {
        Self::default()
    }

    fn next(&mut self) -> T {
        let last = self.next;
        self.next += 1u8.into();
        last
    }

    fn total(self) -> T {
        self.next
    }
}

impl<T> Default for GenId<T>
where
    T: From<u8>,
{
    fn default() -> Self {
        GenId { next: 0u8.into() }
    }
}

fn find_family<'a, B, F>(families: F, qid: QueueFamilyId) -> &'a B::QueueFamily
where
    B: Backend,
    F: IntoIterator<Item = &'a B::QueueFamily>,
{
    families.into_iter().find(|qf| qf.id() == qid).unwrap()
}
