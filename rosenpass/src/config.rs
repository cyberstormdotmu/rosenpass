//! Configuration readable from a config file.
//!
//! Rosenpass supports reading its configuration from a TOML file. This module contains a struct
//! [`Rosenpass`] which holds such a configuration.
//!
//! ## TODO
//! - support `~` in <https://github.com/rosenpass/rosenpass/issues/237>
//! - provide tooling to create config file from shell <https://github.com/rosenpass/rosenpass/issues/247>
use crate::protocol::{SPk, SSk};
use rosenpass_util::file::LoadValue;
use std::{
    collections::HashSet,
    fs,
    io::Write,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs},
    path::{Path, PathBuf},
};

use anyhow::{bail, ensure};
use rosenpass_util::file::{fopen_w, Visibility};
use serde::{Deserialize, Serialize};

use crate::app_server::AppServer;

#[cfg(feature = "experiment_api")]
fn empty_api_config() -> crate::api::config::ApiConfig {
    crate::api::config::ApiConfig {
        listen_path: Vec::new(),
        listen_fd: Vec::new(),
        stream_fd: Vec::new(),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Rosenpass {
    // TODO: Raise error if secret key or public key alone is set during deserialization
    // SEE: https://github.com/serde-rs/serde/issues/2793
    #[serde(flatten)]
    pub keypair: Option<Keypair>,

    /// Location of the API listen sockets
    #[cfg(feature = "experiment_api")]
    #[serde(default = "empty_api_config")]
    pub api: crate::api::config::ApiConfig,

    /// list of [`SocketAddr`] to listen on
    ///
    /// Examples:
    /// - `0.0.0.0:123`
    pub listen: Vec<SocketAddr>,

    /// log verbosity
    ///
    /// This is subject to change. See [`Verbosity`] for details.
    #[serde(default)]
    pub verbosity: Verbosity,

    /// list of peers
    ///
    /// See the [`RosenpassPeer`] type for more information and examples.
    pub peers: Vec<RosenpassPeer>,

    /// path to the file which provided this configuration
    ///
    /// This item is of course not read from the TOML but is added by the algorithm that parses
    /// the config file.
    #[serde(skip)]
    pub config_file_path: PathBuf,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Clone)]
pub struct Keypair {
    /// path to the public key file
    pub public_key: PathBuf,

    /// path to the secret key file
    pub secret_key: PathBuf,
}

impl Keypair {
    pub fn new<Pk: AsRef<Path>, Sk: AsRef<Path>>(public_key: Pk, secret_key: Sk) -> Self {
        let public_key = public_key.as_ref().to_path_buf();
        let secret_key = secret_key.as_ref().to_path_buf();
        Self {
            public_key,
            secret_key,
        }
    }
}

/// ## TODO
/// - replace this type with [`log::LevelFilter`], also see <https://github.com/rosenpass/rosenpass/pull/246>
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Copy, Clone)]
pub enum Verbosity {
    Quiet,
    Verbose,
}

/// ## TODO
/// - examples
/// - documentation
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosenpassPeer {
    /// path to the public key of the peer
    pub public_key: PathBuf,

    /// ## TODO
    /// - documentation
    pub endpoint: Option<String>,

    /// path to the pre-shared key with the peer
    ///
    /// NOTE: this item can be skipped in the config if you do not use a pre-shared key with the peer
    pub pre_shared_key: Option<PathBuf>,

    /// ## TODO
    /// - documentation
    #[serde(default)]
    pub key_out: Option<PathBuf>,

    /// ## TODO
    /// - documentation
    /// - make this field only available on binary builds, not on library builds <https://github.com/rosenpass/rosenpass/issues/249>
    #[serde(flatten)]
    pub wg: Option<WireGuard>,
}

/// ## TODO
/// - documentation
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireGuard {
    /// ## TODO
    /// - documentation
    pub device: String,

    /// ## TODO
    /// - documentation
    pub peer: String,

    /// ## TODO
    /// - documentation
    #[serde(default)]
    pub extra_params: Vec<String>,
}

impl Default for Rosenpass {
    fn default() -> Self {
        Self::empty()
    }
}

impl Rosenpass {
    /// load configuration from a TOML file
    ///
    /// NOTE: no validation is conducted, e.g. the paths specified in the configuration are not
    /// checked whether they even exist.
    ///
    /// ## TODO
    /// - consider using a different algorithm to determine home directory – the below one may
    ///   behave unexpectedly on Windows
    pub fn load<P: AsRef<Path>>(p: P) -> anyhow::Result<Self> {
        // read file and deserialize
        let mut config: Self = toml::from_str(&fs::read_to_string(&p)?)?;

        // resolve `~` (see https://github.com/rosenpass/rosenpass/issues/237)
        use util::resolve_path_with_tilde;
        if let Some(ref mut keypair) = config.keypair {
            resolve_path_with_tilde(&mut keypair.public_key);
            resolve_path_with_tilde(&mut keypair.secret_key);
        }
        for peer in config.peers.iter_mut() {
            resolve_path_with_tilde(&mut peer.public_key);
            if let Some(ref mut psk) = &mut peer.pre_shared_key {
                resolve_path_with_tilde(psk);
            }
            if let Some(ref mut ko) = &mut peer.key_out {
                resolve_path_with_tilde(ko);
            }
        }

        // add path to "self"
        p.as_ref().clone_into(&mut config.config_file_path);

        // return
        Ok(config)
    }

    /// Write a config to a file
    pub fn store<P: AsRef<Path>>(&self, p: P) -> anyhow::Result<()> {
        let serialized_config =
            toml::to_string_pretty(&self).expect("unable to serialize the default config");
        fs::write(p, serialized_config)?;
        Ok(())
    }

    /// Commit the configuration to where it came from, overwriting the original file
    pub fn commit(&self) -> anyhow::Result<()> {
        let mut f = fopen_w(&self.config_file_path, Visibility::Public)?;
        f.write_all(toml::to_string_pretty(&self)?.as_bytes())?;

        self.store(&self.config_file_path)
    }

    pub fn apply_to_app_server(&self, _srv: &mut AppServer) -> anyhow::Result<()> {
        #[cfg(feature = "experiment_api")]
        self.api.apply_to_app_server(_srv)?;
        Ok(())
    }

    /// Validate a configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(ref keypair) = self.keypair {
            // check the public key file exists
            ensure!(
                keypair.public_key.is_file(),
                "could not find public-key file {:?}: no such file. Consider running `rosenpass gen-keys` to generate a new keypair.",
                keypair.public_key
            );

            // check the public-key file is a valid key
            ensure!(
                SPk::load(&keypair.public_key).is_ok(),
                "could not load public-key file {:?}: invalid key",
                keypair.public_key
            );

            // check the secret-key file exists
            ensure!(
                keypair.secret_key.is_file(),
                "could not find secret-key file {:?}: no such file. Consider running `rosenpass gen-keys` to generate a new keypair.",
                keypair.secret_key
            );

            // check the secret-key file is a valid key
            ensure!(
                SSk::load(&keypair.secret_key).is_ok(),
                "could not load public-key file {:?}: invalid key",
                keypair.secret_key
            );
        }

        for (i, peer) in self.peers.iter().enumerate() {
            // check peer's public-key file exists
            ensure!(
                peer.public_key.is_file(),
                "peer {i} public-key file {:?} does not exist",
                peer.public_key
            );

            // check peer's public-key file is a valid key
            ensure!(
                SPk::load(&peer.public_key).is_ok(),
                "peer {i} public-key file {:?} is invalid",
                peer.public_key
            );

            // check endpoint is usable
            if let Some(addr) = peer.endpoint.as_ref() {
                ensure!(
                    addr.to_socket_addrs().is_ok(),
                    "peer {i} endpoint {} can not be parsed to a socket address",
                    addr
                );
            }

            // check if `key_out` or `device` and `peer` are defined
            if peer.key_out.is_none() {
                if let Some(wg) = &peer.wg {
                    if wg.device.is_empty() || wg.peer.is_empty() {
                        ensure!(
                            false,
                            "peer {i} has neither `key_out` nor valid wireguard config defined"
                        );
                    }
                } else {
                    ensure!(
                        false,
                        "peer {i} has neither `key_out` nor valid wireguard config defined"
                    );
                }
            }
        }

        Ok(())
    }

    pub fn check_usefullness(&self) -> anyhow::Result<()> {
        #[cfg(not(feature = "experiment_api"))]
        ensure!(self.keypair.is_some(), "Server keypair missing.");

        #[cfg(feature = "experiment_api")]
        ensure!(
            self.keypair.is_some() || self.api.has_api_sources(),
            "{}{}",
            "Specify a server keypair or some API connections to configure the keypair with.",
            "Without a keypair, rosenpass can not operate."
        );

        Ok(())
    }

    pub fn empty() -> Self {
        Self::new(None)
    }

    pub fn from_sk_pk<Sk: AsRef<Path>, Pk: AsRef<Path>>(sk: Sk, pk: Pk) -> Self {
        Self::new(Some(Keypair::new(pk, sk)))
    }

    /// Creates a new configuration
    pub fn new(keypair: Option<Keypair>) -> Self {
        Self {
            keypair,
            listen: vec![],
            #[cfg(feature = "experiment_api")]
            api: crate::api::config::ApiConfig::default(),
            verbosity: Verbosity::Quiet,
            peers: vec![],
            config_file_path: PathBuf::new(),
        }
    }

    /// Add IPv4 __and__ IPv6 IF_ANY address to the listen interfaces
    pub fn add_if_any(&mut self, port: u16) {
        let ipv4_any = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), port));
        let ipv6_any = SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0),
            port,
            0,
            0,
        ));
        self.listen.push(ipv4_any);
        self.listen.push(ipv6_any);
    }

    /// from chaotic args
    /// Quest: the grammar is undecideable, what do we do here?
    pub fn parse_args(args: Vec<String>) -> anyhow::Result<Self> {
        let mut config = Self::new(Some(Keypair::new("", "")));

        #[derive(Debug, Hash, PartialEq, Eq)]
        enum State {
            Own,
            OwnPublicKey,
            OwnSecretKey,
            OwnListen,
            Peer,
            PeerPsk,
            PeerPublicKey,
            PeerEndpoint,
            PeerOutfile,
            PeerWireguardDev,
            PeerWireguardPeer,
            PeerWireguardExtraArgs,
        }

        let mut already_set = HashSet::new();

        // TODO idea: use config.peers.len() to give index of peer with conflicting argument
        use State::*;
        let mut state = Own;
        let mut current_peer = None;
        let p_exists = "a peer should exist by now";
        let wg_exists = "a peer wireguard should exist by now";
        for arg in args {
            state = match (state, arg.as_str(), &mut current_peer) {
                (Own, "public-key", None) => OwnPublicKey,
                (Own, "secret-key", None) => OwnSecretKey,
                (Own, "private-key", None) => {
                    log::warn!(
                        "the private-key argument is deprecated, please use secret-key instead"
                    );
                    OwnSecretKey
                }
                (Own, "listen", None) => OwnListen,
                (Own, "verbose", None) => {
                    config.verbosity = Verbosity::Verbose;
                    Own
                }
                (Own, "peer", None) => {
                    ensure!(
                        already_set.contains(&OwnPublicKey),
                        "public-key file must be set"
                    );
                    ensure!(
                        already_set.contains(&OwnSecretKey),
                        "secret-key file must be set"
                    );

                    already_set.clear();
                    current_peer = Some(RosenpassPeer::default());

                    Peer
                }
                (OwnPublicKey, pk, None) => {
                    ensure!(
                        already_set.insert(OwnPublicKey),
                        "public-key was already set"
                    );
                    config.keypair.as_mut().unwrap().public_key = pk.into();
                    Own
                }
                (OwnSecretKey, sk, None) => {
                    ensure!(
                        already_set.insert(OwnSecretKey),
                        "secret-key was already set"
                    );
                    config.keypair.as_mut().unwrap().secret_key = sk.into();
                    Own
                }
                (OwnListen, l, None) => {
                    already_set.insert(OwnListen); // multiple listen directives are allowed
                    for socket_addr in l.to_socket_addrs()? {
                        config.listen.push(socket_addr);
                    }

                    Own
                }
                (Peer | PeerWireguardExtraArgs, "peer", maybe_peer @ Some(_)) => {
                    // TODO check current peer
                    // commit current peer, create a new one
                    config.peers.push(maybe_peer.take().expect(p_exists));

                    already_set.clear();
                    current_peer = Some(RosenpassPeer::default());

                    Peer
                }
                (Peer, "public-key", Some(_)) => PeerPublicKey,
                (Peer, "endpoint", Some(_)) => PeerEndpoint,
                (Peer, "preshared-key", Some(_)) => PeerPsk,
                (Peer, "outfile", Some(_)) => PeerOutfile,
                (Peer, "wireguard", Some(_)) => PeerWireguardDev,
                (PeerPublicKey, pk, Some(peer)) => {
                    ensure!(
                        already_set.insert(PeerPublicKey),
                        "public-key was already set"
                    );
                    peer.public_key = pk.into();
                    Peer
                }
                (PeerEndpoint, e, Some(peer)) => {
                    ensure!(already_set.insert(PeerEndpoint), "endpoint was already set");
                    peer.endpoint = Some(e.to_owned());
                    Peer
                }
                (PeerPsk, psk, Some(peer)) => {
                    ensure!(already_set.insert(PeerEndpoint), "peer psk was already set");
                    peer.pre_shared_key = Some(psk.into());
                    Peer
                }
                (PeerOutfile, of, Some(peer)) => {
                    ensure!(
                        already_set.insert(PeerOutfile),
                        "peer outfile was already set"
                    );
                    peer.key_out = Some(of.into());
                    Peer
                }
                (PeerWireguardDev, dev, Some(peer)) => {
                    ensure!(
                        already_set.insert(PeerWireguardDev),
                        "peer wireguard-dev was already set"
                    );
                    assert!(peer.wg.is_none());
                    peer.wg = Some(WireGuard {
                        device: dev.to_string(),
                        ..Default::default()
                    });

                    PeerWireguardPeer
                }
                (PeerWireguardPeer, p, Some(peer)) => {
                    ensure!(
                        already_set.insert(PeerWireguardPeer),
                        "peer wireguard-peer was already set"
                    );
                    peer.wg.as_mut().expect(wg_exists).peer = p.to_string();
                    PeerWireguardExtraArgs
                }
                (PeerWireguardExtraArgs, arg, Some(peer)) => {
                    peer.wg
                        .as_mut()
                        .expect(wg_exists)
                        .extra_params
                        .push(arg.to_string());
                    PeerWireguardExtraArgs
                }

                // error cases
                (Own, x, None) => {
                    bail!("unrecognised argument {x}");
                }
                (Own | OwnPublicKey | OwnSecretKey | OwnListen, _, Some(_)) => {
                    panic!("current_peer is not None while in Own* state, this must never happen")
                }

                (State::Peer, arg, Some(_)) => {
                    bail!("unrecongnised argument {arg}");
                }
                (
                    Peer
                    | PeerEndpoint
                    | PeerOutfile
                    | PeerPublicKey
                    | PeerPsk
                    | PeerWireguardDev
                    | PeerWireguardPeer
                    | PeerWireguardExtraArgs,
                    _,
                    None,
                ) => {
                    panic!("got peer options but no peer was created")
                }
            };
        }

        if let Some(p) = current_peer {
            // TODO ensure peer is propagated with sufficient information
            config.peers.push(p);
        }

        Ok(config)
    }
}

impl Default for Verbosity {
    fn default() -> Self {
        Self::Quiet
    }
}

pub static EXAMPLE_CONFIG: &str = r###"public_key = "/path/to/rp-public-key"
secret_key = "/path/to/rp-secret-key"
listen = []
verbosity = "Verbose"

[[peers]]
# Commented out fields are optional
public_key = "/path/to/rp-peer-public-key"
endpoint = "127.0.0.1:9998"
# pre_shared_key = "/path/to/preshared-key"

# Choose to store the key in a file via `key_out` or pass it to WireGuard by
# defining `device` and `peer`. You may choose to do both.
key_out = "/path/to/rp-key-out.txt" # path to store the key
# device = "wg0" # WireGuard interface
#peer = "RULdRAtUw7SFfVfGD..." # WireGuard public key
# extra_params = [] # passed to WireGuard `wg set`
"###;

#[cfg(test)]
mod test {

    use super::*;
    use std::{borrow::Borrow, net::IpAddr};

    fn toml_des<S: Borrow<str>>(s: S) -> Result<toml::Table, toml::de::Error> {
        toml::from_str(s.borrow())
    }

    fn toml_ser<S: Serialize>(s: S) -> Result<toml::Table, toml::ser::Error> {
        toml::Table::try_from(s)
    }

    fn assert_toml<L: Serialize, R: Borrow<str>>(l: L, r: R, info: &str) -> anyhow::Result<()> {
        fn lines_prepend(prefix: &str, s: &str) -> anyhow::Result<String> {
            use std::fmt::Write;

            let mut buf = String::new();
            for line in s.lines() {
                writeln!(&mut buf, "{prefix}{line}")?;
            }
            Ok(buf)
        }

        let l = toml_ser(l)?;
        let r = toml_des(r.borrow())?;
        ensure!(
            l == r,
            "{}{}TOML value mismatch.\n  Have:\n{}\n  Expected:\n{}",
            info,
            if info.is_empty() { "" } else { ": " },
            lines_prepend("    ", &toml::to_string_pretty(&l)?)?,
            lines_prepend("    ", &toml::to_string_pretty(&r)?)?
        );
        Ok(())
    }

    fn assert_toml_round<'de, L: Serialize + Deserialize<'de>, R: Borrow<str>>(
        l: L,
        r: R,
    ) -> anyhow::Result<()> {
        let l = toml_ser(l)?;
        assert_toml(&l, r.borrow(), "Straight deserialization")?;

        let l: L = l.try_into().unwrap();
        let l = toml_ser(l).unwrap();
        assert_toml(l, r.borrow(), "Roundtrip deserialization")?;

        Ok(())
    }

    fn split_str(s: &str) -> Vec<String> {
        s.split(' ').map(|s| s.to_string()).collect()
    }

    #[test]
    fn toml_serialization() -> anyhow::Result<()> {
        #[cfg(feature = "experiment_api")]
        assert_toml_round(
            Rosenpass::empty(),
            r#"
            listen = []
            verbosity = "Quiet"
            peers = []

            [api]
            listen_path = []
            listen_fd = []
            stream_fd = []
        "#,
        )?;

        #[cfg(not(feature = "experiment_api"))]
        assert_toml_round(
            Rosenpass::empty(),
            r#"
            listen = []
            verbosity = "Quiet"
            peers = []
        "#,
        )?;

        #[cfg(feature = "experiment_api")]
        assert_toml_round(
            Rosenpass::from_sk_pk("/my/sk", "/my/pk"),
            r#"
            public_key = "/my/pk"
            secret_key = "/my/sk"
            listen = []
            verbosity = "Quiet"
            peers = []

            [api]
            listen_path = []
            listen_fd = []
            stream_fd = []
        "#,
        )?;

        #[cfg(not(feature = "experiment_api"))]
        assert_toml_round(
            Rosenpass::from_sk_pk("/my/sk", "/my/pk"),
            r#"
            public_key = "/my/pk"
            secret_key = "/my/sk"
            listen = []
            verbosity = "Quiet"
            peers = []
        "#,
        )?;

        Ok(())
    }

    #[test]
    fn test_simple_cli_parse() {
        let args = split_str(
            "public-key /my/public-key secret-key /my/secret-key verbose \
                listen 0.0.0.0:9999 peer public-key /peer/public-key endpoint \
                peer.test:9999 outfile /peer/rp-out",
        );

        let config = Rosenpass::parse_args(args).unwrap();

        assert_eq!(
            config.keypair,
            Some(Keypair::new("/my/public-key", "/my/secret-key"))
        );
        assert_eq!(config.verbosity, Verbosity::Verbose);
        assert_eq!(
            &config.listen,
            &vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 9999)]
        );
        assert_eq!(
            config.peers,
            vec![RosenpassPeer {
                public_key: PathBuf::from("/peer/public-key"),
                endpoint: Some("peer.test:9999".into()),
                pre_shared_key: None,
                key_out: Some(PathBuf::from("/peer/rp-out")),
                ..Default::default()
            }]
        )
    }

    #[test]
    fn test_cli_parse_multiple_peers() {
        let args = split_str(
            "public-key /my/public-key secret-key /my/secret-key verbose \
                peer public-key /peer-a/public-key endpoint \
                peer.test:9999 outfile /peer-a/rp-out \
                peer public-key /peer-b/public-key outfile /peer-b/rp-out",
        );

        let config = Rosenpass::parse_args(args).unwrap();

        assert_eq!(
            config.keypair,
            Some(Keypair::new("/my/public-key", "/my/secret-key"))
        );
        assert_eq!(config.verbosity, Verbosity::Verbose);
        assert!(&config.listen.is_empty());
        assert_eq!(
            config.peers,
            vec![
                RosenpassPeer {
                    public_key: PathBuf::from("/peer-a/public-key"),
                    endpoint: Some("peer.test:9999".into()),
                    pre_shared_key: None,
                    key_out: Some(PathBuf::from("/peer-a/rp-out")),
                    ..Default::default()
                },
                RosenpassPeer {
                    public_key: PathBuf::from("/peer-b/public-key"),
                    endpoint: None,
                    pre_shared_key: None,
                    key_out: Some(PathBuf::from("/peer-b/rp-out")),
                    ..Default::default()
                }
            ]
        )
    }
}

pub mod util {
    use std::path::PathBuf;
    /// takes a path that can potentially start with a `~` and resolves that `~` to the user's home directory
    ///
    /// ## Example
    /// ```
    /// use rosenpass::config::util::resolve_path_with_tilde;
    /// std::env::set_var("HOME","/home/dummy");
    /// let mut path = std::path::PathBuf::from("~/foo.toml");
    /// resolve_path_with_tilde(&mut path);
    /// assert!(path == std::path::PathBuf::from("/home/dummy/foo.toml"));
    /// ```
    pub fn resolve_path_with_tilde(path: &mut PathBuf) {
        if let Some(first_segment) = path.iter().next() {
            if !path.has_root() && first_segment == "~" {
                let home_dir = home::home_dir().unwrap_or_else(|| {
                    log::error!("config file contains \"~\" but can not determine home diretory");
                    std::process::exit(1);
                });
                let orig_path = path.clone();
                path.clear();
                path.push(home_dir);
                for segment in orig_path.iter().skip(1) {
                    path.push(segment);
                }
            }
        }
    }

    #[cfg(test)]
    mod test {
        use super::*;
        #[test]
        fn test_resolve_path_with_tilde() {
            let test = |path_str: &str, resolved: &str| {
                let mut path = PathBuf::from(path_str);
                resolve_path_with_tilde(&mut path);
                assert!(
                    path == PathBuf::from(resolved),
                    "Path {:?} has been resolved to {:?} but should have been resolved to {:?}.",
                    path_str,
                    path,
                    resolved
                );
            };
            // set environment because otherwise the test result would depend on the system running this
            std::env::set_var("USER", "dummy");
            std::env::set_var("HOME", "/home/dummy");

            // should resolve
            test("~/foo.toml", "/home/dummy/foo.toml");
            test("~//foo", "/home/dummy/foo");
            test("~/../other_user/foo", "/home/dummy/../other_user/foo");

            // should _not_ resolve
            test("~foo/bar", "~foo/bar");
            test(".~/foo", ".~/foo");
            test("/~/foo.toml", "/~/foo.toml");
            test(r"~\foo", r"~\foo");
            test(r"C:\~\foo.toml", r"C:\~\foo.toml");
        }
    }
}
