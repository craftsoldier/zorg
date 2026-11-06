use zcash_protocol::consensus::{BlockHeight, Network, NetworkType, NetworkUpgrade, Parameters};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WalletNetwork {
    Main,
    Test,
    Regtest,
}

impl WalletNetwork {
    /// Every network, mainnet first.
    pub const ALL: [Self; 3] = [Self::Main, Self::Test, Self::Regtest];

    /// The network after this one in [`Self::ALL`], wrapping around.
    pub fn next(self) -> Self {
        let at = Self::ALL.iter().position(|n| *n == self).unwrap_or(0);
        Self::ALL[(at + 1) % Self::ALL.len()]
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Test => "test",
            Self::Regtest => "regtest",
        }
    }

    /// "mainnet", "testnet" or "regtest".
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Main => "mainnet",
            Self::Test => "testnet",
            Self::Regtest => "regtest",
        }
    }

    /// "ZEC" on mainnet, "TAZ" on test networks.
    pub fn unit(&self) -> &'static str {
        match self {
            Self::Main => "ZEC",
            Self::Test | Self::Regtest => "TAZ",
        }
    }

    /// Candidate lightwalletd endpoints, in priority order.
    pub fn default_lightwalletd_urls(&self) -> &'static [&'static str] {
        match self {
            Self::Main => &[
                "https://zec.rocks:443",
                "https://us.zec.stardust.rest:443",
                "https://eu.zec.stardust.rest:443",
                "https://eu.zec.rocks:443",
                "https://na.zec.rocks:443",
                "https://ap.zec.rocks:443",
                "https://sa.zec.rocks:443",
            ],
            Self::Test => &["https://testnet.zec.rocks:443"],
            Self::Regtest => &["http://localhost:9067"],
        }
    }

    /// The network whose Sapling activation height is `height`.
    pub fn from_sapling_activation_height(height: u64) -> Option<Self> {
        Self::ALL.into_iter().find(|net| {
            net.activation_height(NetworkUpgrade::Sapling)
                .is_some_and(|h| u64::from(u32::from(h)) == height)
        })
    }
}

impl From<NetworkType> for WalletNetwork {
    fn from(network: NetworkType) -> Self {
        match network {
            NetworkType::Main => Self::Main,
            NetworkType::Test => Self::Test,
            NetworkType::Regtest => Self::Regtest,
        }
    }
}

impl std::str::FromStr for WalletNetwork {
    type Err = String;

    fn from_str(network: &str) -> Result<Self, Self::Err> {
        match network {
            "main" => Ok(Self::Main),
            "test" => Ok(Self::Test),
            "regtest" => Ok(Self::Regtest),
            _ => Err(format!("Unknown network: {network}")),
        }
    }
}

impl Parameters for WalletNetwork {
    fn network_type(&self) -> NetworkType {
        match self {
            Self::Main => NetworkType::Main,
            Self::Test => NetworkType::Test,
            Self::Regtest => NetworkType::Regtest,
        }
    }

    fn activation_height(&self, nu: NetworkUpgrade) -> Option<BlockHeight> {
        match self {
            Self::Main => Network::MainNetwork.activation_height(nu),
            Self::Test => Network::TestNetwork.activation_height(nu),
            Self::Regtest => match nu {
                NetworkUpgrade::Overwinter
                | NetworkUpgrade::Sapling
                | NetworkUpgrade::Blossom
                | NetworkUpgrade::Heartwood
                | NetworkUpgrade::Canopy
                | NetworkUpgrade::Nu5
                | NetworkUpgrade::Nu6
                | NetworkUpgrade::Nu6_1
                | NetworkUpgrade::Nu6_2
                | NetworkUpgrade::Nu6_3
                | NetworkUpgrade::Nu7 => Some(BlockHeight::from_u32(1)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WalletNetwork;

    #[test]
    fn test_parse_network() {
        assert!(matches!("main".parse(), Ok(WalletNetwork::Main)));
        assert!(matches!("test".parse(), Ok(WalletNetwork::Test)));
        assert!(matches!("regtest".parse(), Ok(WalletNetwork::Regtest)));
        assert!("invalid".parse::<WalletNetwork>().is_err());
    }

    #[test]
    fn sapling_activation_height_identifies_the_network() {
        assert_eq!(
            WalletNetwork::from_sapling_activation_height(419_200),
            Some(WalletNetwork::Main)
        );
        assert_eq!(
            WalletNetwork::from_sapling_activation_height(280_000),
            Some(WalletNetwork::Test)
        );
        assert_eq!(
            WalletNetwork::from_sapling_activation_height(1),
            Some(WalletNetwork::Regtest)
        );
        assert_eq!(WalletNetwork::from_sapling_activation_height(5), None);
    }
}
