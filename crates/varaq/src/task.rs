#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticket(pub u64);
pub trait Scheduler: Send + Sync {
    fn arm(&self) -> Ticket;
    fn wait(&self, ticket: Ticket);
    fn wake(&self, ticket: Ticket);
    fn yield_now(&self);
}
