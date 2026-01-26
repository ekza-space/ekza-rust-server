#[derive(Clone)]
pub struct Services {
    pub echo: EchoService,
}

impl Services {
    pub fn new() -> Self {
        Self {
            echo: EchoService::default(),
        }
    }
}

#[derive(Clone, Default)]
pub struct EchoService;

impl EchoService {
    pub fn echo(&self, message: String) -> String {
        message
    }
}
