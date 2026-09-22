#[derive(Debug)]
pub enum WriteCommand {
    Stage(Vec<String>),
    Unstage(Vec<String>),
}
