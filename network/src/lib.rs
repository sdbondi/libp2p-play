use libp2p::{Multiaddr, PeerId};
use structopt::clap::arg_enum;

arg_enum! {
    #[derive(Debug, Clone)]
    enum Network {
        Foo,
    }
}

impl Network {
    #[rustfmt::skip]
    fn bootnodes(&self) -> Vec<(Multiaddr, PeerId)> {
        match self {
            Network::Foo => {
                vec![
                    ("/ip4/127.0.0.1/tcp/4001".parse().unwrap(), "12D3KooWLUsnhAQzX7gnz9bGaWC9eYL93EnodXprqsB1XM5ffXYe".parse().unwrap()),
                ]
            }
        }
    }

    fn protocol(&self) -> Option<String> {
        match self {
            Network::Foo => Some("/tari/foo/kad".to_string()),
        }
    }
}