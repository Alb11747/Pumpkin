use std::{
    num::NonZero,
    sync::{Arc, atomic::Ordering},
};

use arc_swap::ArcSwap;
use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
use pumpkin_core::{
    data::VanillaData,
    entity::{EntityBase, player::Player},
    net::{
        ClientPlatform, GameProfile, PacketRateLimiter, PlayerConfig,
        java::{JavaClient, pending::PendingConnection},
    },
    server::Server,
};
use pumpkin_data::dimension::Dimension;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_protocol::ConnectionState;
use pumpkin_util::{GameMode, world_seed::Seed};
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

#[tokio::test]
async fn player_persistence_preserves_air_and_updates_independent_historical_xp()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Reuse the real Server/JavaClient fixture pattern from the Player tests.
    let directory = tempfile::tempdir()?;
    let basic = BasicConfiguration {
        default_level_name: directory.path().to_string_lossy().into_owned(),
        seed: Seed(0),
        allow_nether: false,
        allow_end: false,
        allow_chat_reports: false,
        ..BasicConfiguration::default()
    };
    let mut advanced = AdvancedConfiguration::default();
    advanced.networking.bedrock.online_mode = false;
    advanced.networking.java.online_mode = false;
    let distance = NonZero::new(2).ok_or("invalid test view distance")?;
    advanced.networking.java.view_distance = distance;
    advanced.networking.java.simulation_distance = distance;
    let data = VanillaData {
        banned_ip_list: std::sync::RwLock::default(),
        banned_player_list: std::sync::RwLock::default(),
        operator_config: std::sync::RwLock::default(),
        user_cache: std::sync::RwLock::default(),
        whitelist_config: std::sync::RwLock::default(),
    };
    let server = Server::new(
        basic,
        advanced,
        TelemetryConfig::default(),
        data,
        Vec::new(),
    )
    .await?;
    let world = server.get_world_from_dimension(&Dimension::OVERWORLD);
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let (peer, accepted) = tokio::join!(
        TcpStream::connect(listener.local_addr()?),
        listener.accept()
    );
    let _peer = peer?;
    let (stream, address) = accepted?;
    let pending = PendingConnection::new(
        stream,
        address,
        0,
        PacketRateLimiter::new(false, 0.0, 0.0),
        Arc::downgrade(&server),
    );
    let profile = GameProfile {
        id: Uuid::new_v4(),
        name: "PersistenceTest".to_owned(),
        properties: ArcSwap::from_pointee(Vec::new()),
        profile_actions: None,
    };
    let config = PlayerConfig {
        view_distance: distance,
        ..PlayerConfig::default()
    };
    let client = JavaClient::from_pending(pending, profile.clone(), config.clone());
    client.connection_state.store(ConnectionState::Play);
    let player = Arc::new(Player::new(
        Arc::new(ClientPlatform::Java(client)),
        profile,
        config,
        &world,
        GameMode::Survival,
    ));

    let mut imported = NbtCompound::new();
    imported.put_short("Air", 600);
    imported.put_int("XpTotal", 12345);
    imported.put_int("XpLevel", 7);
    imported.put_float("XpP", 0.5);
    player.read_nbt_non_mut(&imported);
    let mut saved = NbtCompound::new();
    player.write_nbt(&mut saved);
    assert_eq!(saved.get_short("Air"), Some(600));
    assert_eq!(saved.get_int("XpTotal"), Some(12345));
    assert_eq!(saved.get_int("XpLevel"), Some(7));
    assert_eq!(saved.get_float("XpP"), Some(0.5));

    player
        .breath_manager
        .air_supply
        .store(599, Ordering::Relaxed);
    player.add_experience_points(7);
    let mut saved = NbtCompound::new();
    player.write_nbt(&mut saved);
    assert_eq!(saved.get_short("Air"), Some(599));
    assert_eq!(saved.get_int("AirSupply"), Some(599));
    assert_eq!(saved.get_int("XpTotal"), Some(12352));

    player.set_experience_level(2, true);
    let mut saved = NbtCompound::new();
    player.write_nbt(&mut saved);
    assert_eq!(saved.get_int("XpTotal"), Some(12352));
    assert_eq!(saved.get_int("XpLevel"), Some(2));

    player.add_experience_levels(-100);
    let mut saved = NbtCompound::new();
    player.write_nbt(&mut saved);
    assert_eq!(saved.get_int("XpTotal"), Some(0));
    assert_eq!(saved.get_int("XpLevel"), Some(0));
    assert_eq!(saved.get_float("XpP"), Some(0.0));
    server.shutdown().await;
    Ok(())
}
