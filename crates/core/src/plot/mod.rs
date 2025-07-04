pub mod commands;
mod data;
pub mod database;
mod monitor;
mod packet_handlers;
mod scoreboard;
pub mod worldedit;

use crate::config::CONFIG;
use crate::interaction;
use crate::interaction::UseOnBlockContext;
use crate::player::{EntityId, Gamemode, PacketSender, Player, PlayerPos};
use crate::server::{BroadcastMessage, Message, PrivMessage};
use crate::utils::HyphenatedUUID;
use anyhow::Error;
use bus::BusReader;
use fpga::scheduler::FPGAScheduler;
use mchprs_blocks::block_entities::BlockEntity;
use mchprs_blocks::blocks::Block;
use mchprs_blocks::items::Item;
use mchprs_blocks::{BlockFace, BlockPos};
use mchprs_network::packets::clientbound::*;
use mchprs_network::packets::serverbound::SUseItemOn;
use mchprs_network::PlayerPacketSender;
use mchprs_backend::{Backend, BackendMsg};
use mchprs_redpiler::{BackendVariant, CompilerOptions};
use mchprs_save_data::plot_data::{ChunkData, PlotData, Tps, WorldSendRate};
use mchprs_text::TextComponent;
use mchprs_world::storage::Chunk;
use mchprs_world::World;
use mchprs_world::{TickEntry, TickPriority};
use monitor::TimingsMonitor;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tracing::{debug, error, warn};

use self::data::sleep_time_for_tps;
use self::scoreboard::Scoreboard;

/// The width of a plot (2^n)
pub const PLOT_SCALE: u32 = 5;

/// The width of a plot counted in chunks
pub const PLOT_WIDTH: i32 = 2i32.pow(PLOT_SCALE);
/// The plot width in blocks
pub const PLOT_BLOCK_WIDTH: i32 = PLOT_WIDTH * 16;
pub const NUM_CHUNKS: usize = PLOT_WIDTH.pow(2) as usize;

/// The height of the world in sections (Default: 24, Max: 127)
pub const PLOT_SECTIONS: usize = 24;
/// The plot height in blocks
pub const PLOT_BLOCK_HEIGHT: i32 = PLOT_SECTIONS as i32 * 16;

const ERROR_IO_ONLY: &str = "This plot cannot be interacted with while redpiler is active with `--io-only`. To stop redpiler, run `/redpiler reset`.";

pub struct Plot {
    pub world: Arc<Mutex<PlotWorld>>,
    pub players: Vec<Player>,
    pub backends: Arc<Mutex<Vec<Backend>>>,
    pub active_backend: Option<usize>,

    backend_rx: Receiver<BackendMsg>,
    backend_tx: Sender<BackendMsg>,


    // Thread communication
    message_receiver: BusReader<BroadcastMessage>,
    message_sender: Sender<Message>,
    priv_message_receiver: Receiver<PrivMessage>,

    locked_players: HashSet<EntityId>,

    // Timings
    tps: Tps,
    world_send_rate: WorldSendRate,
    last_update_time: Instant,
    lag_time: Duration,
    last_nspt: Option<Duration>,
    timings: TimingsMonitor,
    /// The last time a player was in this plot
    last_player_time: Instant,
    /// The last time the world changes were sent to the player
    last_world_send_time: Instant,
    /// The duration we should sleep for after every update
    sleep_time: Duration,
    /// When this is false, the update loop will end and the thread will stop.
    /// This will be set to false if no players are on the plot for a certain amount of time.
    running: bool,
    /// If true, the plot will remain running even if no players are on for a long time.
    always_running: bool,
    auto_redpiler: bool,

    owner: Option<u128>,
    async_rt: Runtime,
    scoreboard: Scoreboard,

    //fpga
    scheduler: Arc<Mutex<FPGAScheduler>>,
}

pub struct PlotWorld {
    pub x: i32,
    pub z: i32,
    pub chunks: Vec<Chunk>,
    pub to_be_ticked: Vec<TickEntry>,
    pub packet_senders: Vec<PlayerPacketSender>,
}

impl PlotWorld {
    fn get_plot (&self) -> (i32,i32) {
        (self.x, self.z)
    }

    fn get_chunk_index_for_chunk(&self, chunk_x: i32, chunk_z: i32) -> usize {
        let local_x = chunk_x - self.x * PLOT_WIDTH;
        let local_z = chunk_z - self.z * PLOT_WIDTH;
        (local_x * PLOT_WIDTH + local_z).unsigned_abs() as usize
    }

    fn get_chunk_index_for_block(&self, block_x: i32, block_z: i32) -> Option<usize> {
        let chunk_x = (block_x - (self.x * PLOT_BLOCK_WIDTH)) >> 4;
        let chunk_z = (block_z - (self.z * PLOT_BLOCK_WIDTH)) >> 4;
        if chunk_x >= PLOT_WIDTH || chunk_z >= PLOT_WIDTH {
            return None;
        }
        Some(((chunk_x << PLOT_SCALE) + chunk_z).unsigned_abs() as usize)
    }

    fn flush_block_changes(&mut self) {
        for packet in self.chunks.iter_mut().flat_map(|c| c.multi_blocks()) {
            let encoded = packet.encode();
            for player in &self.packet_senders {
                player.send_packet(&encoded);
            }
        }
        for chunk in &mut self.chunks {
            chunk.reset_multi_blocks();
        }
    }

    pub fn get_corners(&self) -> (BlockPos, BlockPos) {
        const W: i32 = PLOT_BLOCK_WIDTH;
        let first_pos = BlockPos::new(self.x * W, 0, self.z * W);
        let second_pos = BlockPos::new(
            (self.x + 1) * W - 1,
            PLOT_BLOCK_HEIGHT - 1,
            (self.z + 1) * W - 1,
        );
        (first_pos, second_pos)
    }
}

impl World for PlotWorld {
    /// Sets a block in storage. Returns true if a block was changed.
    fn set_block_raw(&mut self, pos: BlockPos, block: u32) -> bool {
        let chunk_index = match self.get_chunk_index_for_block(pos.x, pos.z) {
            Some(idx) => idx,
            None => return false,
        };

        // Check to see if block is within height limit
        if pos.y >= PLOT_BLOCK_HEIGHT || pos.y < 0 {
            return false;
        }

        let chunk = &mut self.chunks[chunk_index];
        chunk.set_block(
            (pos.x & 0xF) as u32,
            pos.y as u32,
            (pos.z & 0xF) as u32,
            block,
        )
    }

    /// Returns the block state id of the block at `pos`
    fn get_block_raw(&self, pos: BlockPos) -> u32 {
        let chunk_index = match self.get_chunk_index_for_block(pos.x, pos.z) {
            Some(idx) => idx,
            None => return 0,
        };
        let chunk = &self.chunks[chunk_index];
        chunk.get_block((pos.x & 0xF) as u32, pos.y as u32, (pos.z & 0xF) as u32)
    }

    fn delete_block_entity(&mut self, pos: BlockPos) {
        let chunk_index = match self.get_chunk_index_for_block(pos.x, pos.z) {
            Some(idx) => idx,
            None => return,
        };
        let chunk = &mut self.chunks[chunk_index];
        chunk.delete_block_entity(BlockPos::new(pos.x & 0xF, pos.y, pos.z & 0xF));
    }

    fn get_block_entity(&self, pos: BlockPos) -> Option<&BlockEntity> {
        let chunk_index = match self.get_chunk_index_for_block(pos.x, pos.z) {
            Some(idx) => idx,
            None => return None,
        };
        let chunk = &self.chunks[chunk_index];
        chunk.get_block_entity(BlockPos::new(pos.x & 0xF, pos.y, pos.z & 0xF))
    }

    fn set_block_entity(&mut self, pos: BlockPos, block_entity: BlockEntity) {
        let chunk_index = match self.get_chunk_index_for_block(pos.x, pos.z) {
            Some(idx) => idx,
            None => return,
        };
        if let Some(nbt) = block_entity.to_nbt(true) {
            let block_entity_data = CBlockEntityData {
                x: pos.x,
                y: pos.y,
                z: pos.z,
                // For now the only nbt we send to the client is sign data
                ty: block_entity.ty(),
                nbt: nbt.content,
            }
            .encode();
            for player in &self.packet_senders {
                player.send_packet(&block_entity_data);
            }
        }
        let chunk = &mut self.chunks[chunk_index];
        chunk.set_block_entity(BlockPos::new(pos.x & 0xF, pos.y, pos.z & 0xF), block_entity);
    }

    fn get_chunk(&self, x: i32, z: i32) -> Option<&Chunk> {
        self.chunks.get(self.get_chunk_index_for_chunk(x, z))
    }

    fn get_chunk_mut(&mut self, x: i32, z: i32) -> Option<&mut Chunk> {
        let chunk_idx = self.get_chunk_index_for_chunk(x, z);
        self.chunks.get_mut(chunk_idx)
    }

    fn schedule_tick(&mut self, pos: BlockPos, delay: u32, priority: TickPriority) {
        self.to_be_ticked.push(TickEntry {
            pos,
            ticks_left: delay,
            tick_priority: priority,
        });
    }

    fn pending_tick_at(&mut self, pos: BlockPos) -> bool {
        self.to_be_ticked.iter().any(|e| e.pos == pos)
    }

    fn play_sound(
        &mut self,
        pos: BlockPos,
        sound_id: i32,
        sound_category: i32,
        volume: f32,
        pitch: f32,
    ) {
        // FIXME: We do not know the players location here, so we send the sound packet to all players
        // A notchian server would only send to players in hearing distance (volume.clamp(0.0, 1.0) * 16.0)
        let sound_effect_data = CSoundEffect {
            sound_id,
            sound_name: None,
            has_fixed_range: None,
            range: None,
            sound_category,
            x: pos.x * 8 + 4,
            y: pos.y * 8 + 4,
            z: pos.z * 8 + 4,
            volume,
            pitch,
            // FIXME: How do we decide this?
            seed: 0,
        }
        .encode();

        for player in &self.packet_senders {
            player.send_packet(&sound_effect_data);
        }
    }
}

impl Plot {
    fn tickn(&mut self, ticks: u64) {
        if !self.active_backend.is_none() {
            self.timings.tickn(ticks);
            self.backends.lock().unwrap()[self.active_backend.unwrap()].tickn(ticks);
            return;
        }

        for _ in 0..ticks {
            self.tick();
        }
    }

    fn tick(&mut self) {
        self.timings.tick();
        if !self.active_backend.is_none() {
            self.backends.lock().unwrap()[self.active_backend.unwrap()].tick();
            return;
        }

        let mut world = self.world.lock().unwrap();
        world
            .to_be_ticked
            .sort_by_key(|e| (e.ticks_left, e.tick_priority));
        for pending in &mut world.to_be_ticked {
            pending.ticks_left = pending.ticks_left.saturating_sub(1);
        }
        while world.to_be_ticked.first().map_or(1, |e| e.ticks_left) == 0 {
            let entry = world.to_be_ticked.remove(0);
            mchprs_redstone::tick(world.get_block(entry.pos), &mut *world, entry.pos);
        }
    }

    /// Send a block change to all connected players
    pub fn send_block_change(&mut self, pos: BlockPos, id: u32) {
        let block_change = CBlockUpdate {
            block_id: id as i32,
            x: pos.x,
            y: pos.y,
            z: pos.z,
        }
        .encode();
        for player in &mut self.players {
            player.client.send_packet(&block_change);
        }
    }
    
    pub fn broadcast_chat_message(&mut self, message: String) {
        let world = self.world.lock().unwrap();
        let broadcast_message = Message::ChatInfo(
            0,
            format!("Plot {}-{}", world.x, world.z),
            message,
        );
        self.message_sender.send(broadcast_message).unwrap();
    }

    pub fn broadcast_plot_chat_message(&mut self, message: &str) {
        for player in &mut self.players {
            player.send_chat_message(&TextComponent::from_legacy_text(message));
        }
    }

    fn change_player_gamemode(&mut self, player_idx: usize, gamemode: Gamemode) {
        self.players[player_idx].set_gamemode(gamemode);
        let _ = self.message_sender.send(Message::PlayerUpdateGamemode(
            self.players[player_idx].uuid,
            gamemode,
        ));
    }

    fn on_player_move(&mut self, player_idx: usize, old: PlayerPos, new: PlayerPos) {
        let old_block_pos = old.block_pos();
        let new_block_pos = new.block_pos();
        
        let old_block = { self.world.lock().unwrap().get_block(old_block_pos) };
        
        let new_block = { self.world.lock().unwrap().get_block(new_block_pos) };

        if let Block::StonePressurePlate { powered: true } = old_block {
            if !self.are_players_on_block(old_block_pos) {
                self.set_pressure_plate(old_block_pos, false);
            }
        }

        if let Block::StonePressurePlate { powered: false } = new_block {
            if self.players[player_idx].on_ground {
                self.set_pressure_plate(new_block_pos, true);
            }
        }
    }

    fn set_pressure_plate(&mut self, pos: BlockPos, powered: bool) {

        if !self.active_backend.is_none() {
            self.backends.lock().unwrap()[self.active_backend.unwrap()].set_pressure_plate(pos, powered);
            return;
        }

        let mut world = self.world.lock().unwrap();
        let block = world.get_block(pos);
        match block {
            Block::StonePressurePlate { .. } => {
                world
                    .set_block(pos, Block::StonePressurePlate { powered });
                mchprs_redstone::update_surrounding_blocks(&mut *world, pos);
                mchprs_redstone::update_surrounding_blocks(
                    &mut *world,
                    pos.offset(BlockFace::Bottom),
                );
            }
            _ => warn!("Block at {} is not a pressure plate", pos),
        }
    }

    fn are_players_on_block(&mut self, pos: BlockPos) -> bool {
        for player in &self.players {
            if player.pos.block_pos() == pos && player.on_ground {
                return true;
            }
        }
        false
    }

    fn enter_plot(&mut self, player: Player) {
        self.save();
        let spawn_player = player.spawn_packet().encode();
        let metadata = player.metadata_packet().encode();
        let entity_equipment = player.equippment_packet();
        for other_player in &mut self.players {
            other_player.client.send_packet(&spawn_player);
            other_player.client.send_packet(&metadata);
            if let Some(entity_equipment) = &entity_equipment {
                other_player.client.send_packet(&entity_equipment.encode());
            }

            let spawn_other_player = other_player.spawn_packet().encode();
            player.client.send_packet(&spawn_other_player);

            if let Some(other_entity_equipment) = other_player.equippment_packet() {
                player.client.send_packet(&other_entity_equipment.encode());
            }

            let other_metadata = other_player.metadata_packet().encode();
            player.client.send_packet(&other_metadata);
        }
        {
            let world = self.world.lock().unwrap();
            player.send_system_message(&format!(
                "Entering plot ({}, {})",
                world.x, world.z
            ));
        }
        self.world.lock().unwrap()
            .packet_senders
            .push(PlayerPacketSender::new(&player.client));
        self.scoreboard.add_player(&player);
        self.players.push(player);
        self.update_view_pos_for_player(self.players.len() - 1, true);
    }

    fn get_chunk_distance(x1: i32, z1: i32, x2: i32, z2: i32) -> u32 {
        let x = x1 - x2;
        let z = z1 - z2;
        x.abs().max(z.abs()) as u32
    }

    fn set_chunk_loaded_at_player(
        &mut self,
        player_idx: usize,
        chunk_x: i32,
        chunk_z: i32,
        was_loaded: bool,
        should_be_loaded: bool,
    ) {
        if was_loaded && !should_be_loaded {
            // let unload_chunk = CUnloadChunk { chunk_x, chunk_z }.encode();
            // self.players[player_idx].client.send_packet(&unload_chunk);
        } else if !was_loaded && should_be_loaded {
            let world = self.world.lock().unwrap();
            if !Plot::chunk_in_plot_bounds(world.x, world.z, chunk_x, chunk_z) {
                self.players[player_idx]
                    .client
                    .send_packet(&Chunk::encode_empty_packet(chunk_x, chunk_z, PLOT_SECTIONS));
            } else {
                let chunk_data = world.chunks
                    [world.get_chunk_index_for_chunk(chunk_x, chunk_z)]
                .encode_packet();
                self.players[player_idx].client.send_packet(&chunk_data);
            }
        }
    }

    pub fn update_view_pos_for_player(&mut self, player_idx: usize, force_load: bool) {
        let view_distance = CONFIG.view_distance as i32;
        let (chunk_x, chunk_z) = self.players[player_idx].pos.chunk_pos();
        let last_chunk_x = self.players[player_idx].last_chunk_x;
        let last_chunk_z = self.players[player_idx].last_chunk_z;

        let update_view = CSetCenterChunk { chunk_x, chunk_z }.encode();
        self.players[player_idx].client.send_packet(&update_view);

        if ((last_chunk_x - chunk_x).abs() <= view_distance * 2
            && (last_chunk_z - chunk_z).abs() <= view_distance * 2)
            && !force_load
        {
            let nx = chunk_x.min(last_chunk_x) - view_distance;
            let nz = chunk_z.min(last_chunk_z) - view_distance;
            let px = chunk_x.max(last_chunk_x) + view_distance;
            let pz = chunk_z.max(last_chunk_z) + view_distance;
            for x in nx..=px {
                for z in nz..=pz {
                    let was_loaded = Self::get_chunk_distance(x, z, last_chunk_x, last_chunk_z)
                        <= view_distance as u32;
                    let should_be_loaded =
                        Self::get_chunk_distance(x, z, chunk_x, chunk_z) <= view_distance as u32;
                    self.set_chunk_loaded_at_player(player_idx, x, z, was_loaded, should_be_loaded);
                }
            }
        } else {
            for x in last_chunk_x - view_distance..=last_chunk_x + view_distance {
                for z in last_chunk_z - view_distance..=last_chunk_z + view_distance {
                    self.set_chunk_loaded_at_player(player_idx, x, z, true, false);
                }
            }
            for x in chunk_x - view_distance..=chunk_x + view_distance {
                for z in chunk_z - view_distance..=chunk_z + view_distance {
                    self.set_chunk_loaded_at_player(player_idx, x, z, false, true);
                }
            }
        }
        self.players[player_idx].last_chunk_x = chunk_x;
        self.players[player_idx].last_chunk_z = chunk_z;
    }

    fn cancel (&mut self, block_pos: BlockPos, block_face: BlockFace) {
        let block = { self.world.lock().unwrap().get_block_raw(block_pos) };
        self.send_block_change(block_pos, block);

        let offset_pos = block_pos.offset(block_face);
        let offset_block = { self.world.lock().unwrap().get_block_raw(offset_pos) };
        self.send_block_change(offset_pos, offset_block);
    }

    fn handle_use_item_impl(&mut self, use_item_on: &SUseItemOn, player: usize) {
        let block_pos = BlockPos::new(use_item_on.x, use_item_on.y, use_item_on.z);
        let block_face = BlockFace::from_id(use_item_on.face as u32);

        let selected_slot = self.players[player].selected_slot as usize;
        let item_in_hand = if use_item_on.hand == 0 {
            // Slot in hotbar
            self.players[player].inventory[selected_slot + 36].clone()
        } else {
            // Slot for left hand
            self.players[player].inventory[45].clone()
        };

        let in_bounds = {
            let world = self.world.lock().unwrap();
            !Plot::in_plot_bounds(world.x, world.z, block_pos.x, block_pos.z)
        };
        if in_bounds {
            self.players[player].send_system_message("Can't interact with blocks outside of plot");
            self.cancel(block_pos, block_face);
            return;
        }

        if let Some(item) = &item_in_hand {
            let has_permission = self.players[player].has_permission("worldedit.selection.pos");
            if item.item_type == (Item::WEWand {}) && has_permission {
                let same = self.players[player]
                    .second_position
                    .map_or(false, |p| p == block_pos);
                if !same {
                    self.players[player].worldedit_set_second_position(block_pos);
                }
                self.cancel(block_pos, block_face);
                // FIXME: Because the client sends another packet after this for the left hand for most blocks,
                // redpiler will get reset anyways.
                return;
            }
        }

        if let Some(owner) = self.owner {
            let player = &mut self.players[player];
            if owner != player.uuid && !player.has_permission("plots.admin.interact.other") {
                player.send_no_permission_message();
                self.cancel(block_pos, block_face);
                return;
            }
        } else if !self.players[player].has_permission("plots.admin.interact.unowned") {
            self.players[player].send_no_permission_message();
            self.cancel(block_pos, block_face);
            return;
        }

        if !self.active_backend.is_none() {
            let lever_or_button = {
                let world = self.world.lock().unwrap();
                let block = world.get_block(block_pos);
                matches!(block, Block::Lever { .. } | Block::StoneButton { .. })
            };
            if lever_or_button && !self.players[player].crouching {
                { self.backends.lock().unwrap()[self.active_backend.unwrap()].on_use_block(block_pos); }
                let mut world = self.world.lock().unwrap();
                { self.backends.lock().unwrap()[self.active_backend.unwrap()].flush(&mut *world); }
                world.flush_block_changes();
                return;
            } else {
                if self.is_io_only() {
                    self.players[player].send_error_message(ERROR_IO_ONLY);
                    self.cancel(block_pos, block_face);
                    return;
                }
                self.reset_backend();
            }
        }

        if let Some(item) = item_in_hand {
            let cancelled = interaction::use_item_on_block(
                &item,
                &mut self.world.lock().unwrap(),
                UseOnBlockContext {
                    block_face,
                    block_pos,
                    player: &mut self.players[player],
                    cursor_y: use_item_on.cursor_y,
                },
            );
            if cancelled {
                self.cancel(block_pos, block_face);
            }
            self.world.lock().unwrap().flush_block_changes();
            return;
        }

        let block = { self.world.lock().unwrap().get_block(block_pos) };
        if !self.players[player].crouching {
            interaction::on_use(
                block,
                &mut *self.world.lock().unwrap(),
                &mut self.players[player],
                block_pos,
                None,
            );
            self.world.lock().unwrap().flush_block_changes();
        }
    }

    fn handle_player_digging(&mut self, block_pos: BlockPos, player: usize) {
       
        let block = { self.world.lock().unwrap().get_block(block_pos) };
        {
            let world = self.world.lock().unwrap();
            if !Plot::in_plot_bounds(world.x, world.z, block_pos.x, block_pos.z) {
                self.players[player].send_system_message("Can't break blocks outside of plot");
                return;
            }
        }

        // This worldedit wand stuff should probably be done in another file. It's good enough for now.
        let item_in_hand = self.players[player].inventory
            [self.players[player].selected_slot as usize + 36]
            .clone();
        if let Some(item) = item_in_hand {
            let has_permission = self.players[player].has_permission("worldedit.selection.pos");
            if item.item_type == (Item::WEWand {}) && has_permission {
                self.send_block_change(block_pos, block.get_id());
                if let Some(pos) = self.players[player].first_position {
                    if pos == block_pos {
                        return;
                    }
                }
                self.players[player].worldedit_set_first_position(block_pos);
                return;
            }
        }

        if let Some(owner) = self.owner {
            let player = &mut self.players[player];
            if owner != player.uuid && !player.has_permission("plots.admin.interact.other") {
                player.send_no_permission_message();
                self.send_block_change(block_pos, block.get_id());
                return;
            }
        } else if !self.players[player].has_permission("plots.admin.interact.unowned") {
            self.players[player].send_no_permission_message();
            self.send_block_change(block_pos, block.get_id());
            return;
        }

        if !self.active_backend.is_none() && self.backends.lock().unwrap()[self.active_backend.unwrap()].options().io_only {
            self.players[player].send_error_message(ERROR_IO_ONLY);
            self.send_block_change(block_pos, block.get_id());
            return;
        }
       
        self.reset_backend();

        let mut world = self.world.lock().unwrap();
        interaction::destroy(block, &mut *world, block_pos);
        world.flush_block_changes();

        let effect = CWorldEvent {
            event: 2001,
            x: block_pos.x,
            y: block_pos.y,
            z: block_pos.z,
            data: block.get_id() as i32,
            disable_relative_volume: false,
        }
        .encode();
        for other_player in 0..self.players.len() {
            if player == other_player {
                continue;
            };
            self.players[other_player].client.send_packet(&effect);
        }
    }

    /// After an expensive operation or change in timings, it's important to
    /// call this function so our timings monitor doesn't think we're running
    /// behind.
    fn reset_timings(&mut self) {
        self.lag_time = Duration::ZERO;
        self.last_update_time = Instant::now();
        self.last_nspt = None;
        self.timings.reset_timings();
    }

    fn start_backend(&mut self, options: CompilerOptions, name: String, player: usize) {
        debug!("Starting redpiler");

        let plr: &Player = &self.players[player];
        let bounds = if options.selection {
                let pos = (plr.first_position, plr.second_position);
                if pos.0.is_some() && pos.1.is_some() {
                    (pos.0.unwrap(), pos.1.unwrap())
                } else {
                    self.world.lock().unwrap().get_corners()
                }
            } else {
                self.world.lock().unwrap().get_corners()
            }.clone(); 
        let config = if options.backend_variant == BackendVariant::FPGA {
                Some(self.scheduler.lock().unwrap().get_config())
            }
            else {
                None
            };
        let ticks = { self.world.lock().unwrap().to_be_ticked.drain(..).collect() };
        let world = Arc::clone(&self.world);
        let backends: Arc<Mutex<Vec<Backend>>> = Arc::clone(&self.backends);
        let sender = self.backend_tx.clone();
        let x = { self.world.lock().unwrap().x };
        let z = { self.world.lock().unwrap().z };

        thread::spawn(move || {
            let new_backend: Backend = Backend::new(
                sender,
                name,
                format!("{}-{}", x, z),
                config,
                &world,
                bounds,  
                options,
                ticks);
            backends.lock().unwrap().push(new_backend);
        });

        self.reset_timings();
    }

    fn reset_backend(&mut self) {

        if !self.active_backend.is_none() {
            debug!("Stopping Backend");
            let bounds = { self.world.lock().unwrap().get_corners() };
            self.backends.lock().unwrap()[self.active_backend.unwrap()].reset(&mut *self.world.lock().unwrap(), bounds);
            self.active_backend = None;

            // reseting redpiler could cause a large amount of block updates
            self.reset_timings();

        }
        else {
            debug!("No active backend");
        }
    }

    fn is_io_only(&mut self) -> bool {
        if !self.active_backend.is_none() {
            self.backends.lock().unwrap()[self.active_backend.unwrap()].options().io_only
        }
        else {
            false
        }
    }

    fn destroy_entity(&mut self, entity_id: u32) {
        let destroy_entity = CRemoveEntities {
            entity_ids: vec![entity_id as i32],
        }
        .encode();
        for player in &mut self.players {
            player.client.send_packet(&destroy_entity);
        }
    }

    fn leave_plot(&mut self, uuid: u128) -> Player {
        let player_idx = self.players.iter().position(|p| p.uuid == uuid).unwrap();
        { self.world.lock().unwrap().packet_senders.remove(player_idx); }
        let player = self.players.remove(player_idx);

        let destroy_other_entities = CRemoveEntities {
            entity_ids: self.players.iter().map(|p| p.entity_id as i32).collect(),
        }
        .encode();
        player.client.send_packet(&destroy_other_entities);
        {
            let world = self.world.lock().unwrap();
            let chunk_offset_x = world.x << PLOT_SCALE;
            let chunk_offset_z = world.z << PLOT_SCALE;
            for chunk in &world.chunks {
                player.client.send_packet(
                    &CUnloadChunk {
                        chunk_x: chunk_offset_x + chunk.x,
                        chunk_z: chunk_offset_z + chunk.z,
                    }
                    .encode(),
                );
            }
        }
        self.destroy_entity(player.entity_id);
        self.locked_players.remove(&player.entity_id);
        self.scoreboard.remove_player(&player);
        player
    }

    fn chunk_in_plot_bounds(plot_x: i32, plot_z: i32, chunk_x: i32, chunk_z: i32) -> bool {
        let (x, z) = (chunk_x >> PLOT_SCALE, chunk_z >> PLOT_SCALE);
        plot_x == x && plot_z == z
    }

    fn in_plot_bounds(plot_x: i32, plot_z: i32, x: i32, z: i32) -> bool {
        Plot::chunk_in_plot_bounds(plot_x, plot_z, x >> 4, z >> 4)
    }

    pub fn claim_plot(&mut self, plot_x: i32, plot_z: i32, player: usize) {
        let player = &mut self.players[player];
        database::claim_plot(plot_x, plot_z, &format!("{:032x}", player.uuid));
        let center = Plot::get_center(plot_x, plot_z);
        player.teleport(PlayerPos::new(center.0, 64.0, center.1));
        player.send_system_message(&format!("Claimed plot {},{}", plot_x, plot_z));
    }

    pub fn get_center(plot_x: i32, plot_z: i32) -> (f64, f64) {
        const WIDTH: f64 = PLOT_BLOCK_WIDTH as f64;
        (
            plot_x as f64 * WIDTH + WIDTH / 2.0,
            plot_z as f64 * WIDTH + WIDTH / 2.0,
        )
    }

    pub fn get_next_plot(plot_x: i32, plot_z: i32) -> (i32, i32) {
        let x = plot_x.abs();
        let z = plot_z.abs();

        match x.cmp(&z) {
            Ordering::Greater => {
                if plot_x > 0 {
                    (plot_x, plot_z + 1)
                } else {
                    (plot_x, plot_z - 1)
                }
            }
            Ordering::Less => {
                if plot_z > 0 {
                    (plot_x - 1, plot_z)
                } else {
                    (plot_x + 1, plot_z)
                }
            }
            Ordering::Equal => {
                if plot_x == plot_z && plot_x > 0 || plot_x == x {
                    (plot_x, plot_z + 1)
                } else if plot_z == z {
                    (plot_x, plot_z - 1)
                } else {
                    (plot_x + 1, plot_z)
                }
            }
        }
    }

    fn handle_commands(&mut self) {
        let mut removal_offset = 0;
        for player_idx in 0..self.players.len() {
            let player_idx = player_idx - removal_offset;
            let commands: Vec<String> = self.players[player_idx].command_queue.drain(..).collect();
            for command in commands {
                let mut args: Vec<&str> = command.split(' ').collect();
                let command = args.remove(0);
                if self.handle_command(player_idx, command, args) {
                    removal_offset += 1;
                }
            }
        }
    }

    fn handle_messages(&mut self) {
        while let Ok(message) = self.message_receiver.try_recv() {
            match message {
                BroadcastMessage::Chat(_sender, message) => {
                    for player in &mut self.players {
                        player.send_chat_message(&message);
                    }
                }
                BroadcastMessage::PlayerJoinedInfo(player_join_info) => {
                    let player_info = CPlayerInfoUpdate {
                        players: vec![CPlayerInfoUpdatePlayer {
                            uuid: player_join_info.uuid,
                            actions: {
                                let mut actions: CPlayerInfoActions = Default::default();
                                actions.add_player = Some(CPlayerInfoAddPlayer {
                                    name: player_join_info.username,
                                    properties: player_join_info.properties,
                                });
                                actions.update_gamemode = Some(player_join_info.gamemode.get_id());
                                actions
                            },
                        }],
                    }
                    .encode();
                    for player in &mut self.players {
                        player.client.send_packet(&player_info);
                    }
                }
                BroadcastMessage::PlayerLeft(uuid) => {
                    let player_info = CPlayerInfoRemove {
                        players: vec![uuid],
                    }
                    .encode();
                    for player in &mut self.players {
                        player.client.send_packet(&player_info);
                    }
                }
                BroadcastMessage::Shutdown => {
                    let mut players: Vec<Player> = self.players.drain(..).collect();
                    for player in players.iter_mut() {
                        player.save();
                        player.kick("Server closed".into());
                    }
                    self.always_running = false;
                    self.running = false;
                    return;
                }
                BroadcastMessage::PlayerUpdateGamemode(uuid, gamemode) => {
                    let player_info = CPlayerInfoUpdate {
                        players: vec![CPlayerInfoUpdatePlayer {
                            uuid,
                            actions: {
                                let mut actions: CPlayerInfoActions = Default::default();
                                actions.update_gamemode = Some(gamemode.get_id());
                                actions
                            },
                        }],
                    }
                    .encode();
                    for player in &mut self.players {
                        player.client.send_packet(&player_info);
                    }
                }
            }
        }
        // Handle messages from the private message channel
        while let Ok(message) = self.priv_message_receiver.try_recv() {
            match message {
                PrivMessage::PlayerEnterPlot(player) => {
                    self.enter_plot(player);
                }
                PrivMessage::PlayerTeleportOther(mut player, username) => {
                    if let Some(other) = self.players.iter().find(|p| p.username == username) {
                        player.teleport(other.pos);
                    }
                    self.enter_plot(player);
                }
            }
        }
    }

    /// Remove players outside of the plot
    fn remove_oob_players(&mut self) {
        let mut outside_players = Vec::new();
        for player in 0..self.players.len() {
            let player = &mut self.players[player];
            if self.locked_players.contains(&player.entity_id) {
                continue;
            }
            let (plot_x, plot_z) = player.pos.plot_pos();

            let world = self.world.lock().unwrap();
            if plot_x != world.x || plot_z != world.z {
                outside_players.push(player.uuid);
            }
        }

        for uuid in outside_players {
            let player = self.leave_plot(uuid);
            let player_leave_plot = Message::PlayerLeavePlot(player);
            self.message_sender.send(player_leave_plot).unwrap();
        }
    }

    /// Remove disconnected players
    fn remove_dc_players(&mut self) {
        let message_sender = &mut self.message_sender;

        let mut disconnected_players = Vec::new();
        self.players.retain(|player| {
            let alive = player.client.alive();
            if !alive {
                player.save();
                message_sender
                    .send(Message::PlayerLeft(player.uuid))
                    .unwrap();
                disconnected_players.push(player.entity_id);
            }
            alive
        });
        for entity_id in disconnected_players {
            self.destroy_entity(entity_id);
        }
    }

    /// Update player view positions and handle packets
    fn update_players(&mut self) {
        for player_idx in 0..self.players.len() {
            if self.players[player_idx].update() {
                self.update_view_pos_for_player(player_idx, false);
            }
        }
        // Handle received packets
        for player_idx in 0..self.players.len() {
            self.handle_packets_for_player(player_idx);
        }
    }

    fn update(&mut self) {
        self.handle_messages();

        let mut new_sb = false;
        while let Ok(message) = self.backend_rx.try_recv() {
            self.scoreboard.parse_scoreboard_msg(message);
            new_sb = true;
        }
        if new_sb {
            self.scoreboard.update(&self.players);
        }

        // Only tick if there are players in the plot
        if !self.players.is_empty() {

            self.timings.set_ticking(true);
            let now = Instant::now();
            self.last_player_time = now;

            let world_send_rate =
                Duration::from_nanos(1_000_000_000 / self.world_send_rate.0 as u64);

            let max_batch_size = match self.last_nspt {
                Some(Duration::ZERO) | None => 1,
                Some(last_nspt) => {
                    let ticks_fit = (world_send_rate.as_nanos() / last_nspt.as_nanos()) as u64;
                    // A tick previously took longer than the world send rate.
                    // Run at least one just so we're not stuck doing nothing
                    ticks_fit.max(1)
                }
            };

            let batch_size = match self.tps {
                Tps::Limited(tps) if tps != 0 => {
                    let dur_per_tick = Duration::from_nanos(1_000_000_000 / tps as u64);
                    self.lag_time += now - self.last_update_time;
                    let batch_size = (self.lag_time.as_nanos() / dur_per_tick.as_nanos()) as u64;
                    self.lag_time -= dur_per_tick * batch_size as u32;
                    batch_size.min(max_batch_size)
                }
                Tps::Unlimited => max_batch_size,
                _ => 0,
            };

            self.last_update_time = now;
            if batch_size != 0 {
                // 50_000 (= 3.33 MHz) here is arbitrary.
                // We just need a number that's not too high so we actually get around to sending block updates.
                let batch_size = batch_size.min(50_000) as u32;
                let mut ticks_completed = batch_size;
                if !self.active_backend.is_none() {
                    self.tickn(batch_size as u64);
                    self.backends.lock().unwrap()[self.active_backend.unwrap()].flush(&mut *self.world.lock().unwrap());
                } else {
                    for i in 0..batch_size {
                        self.tick();
                        if now.elapsed() > Duration::from_millis(200) {
                            ticks_completed = i + 1;
                            break;
                        }
                    }
                }
                self.last_nspt = Some(self.last_update_time.elapsed() / ticks_completed);
            }

            // if self.auto_redpiler
            //     && !self.is_rp_active()
            //     && (self.tps == Tps::Unlimited || self.timings.is_running_behind())
            // {
            //     self.start_backend(Default::default(), 0);
            // }

            let now = Instant::now();
            let time_since_last_world_send = now - self.last_world_send_time;
            if time_since_last_world_send > world_send_rate {
                self.last_world_send_time = now;
                self.world.lock().unwrap().flush_block_changes();
            }
        } else {
            self.timings.set_ticking(false);
            // Unload plot after 600 seconds unless the plot should be always loaded
            if self.last_player_time.elapsed().as_secs() > 600 && !self.always_running {
                self.running = false;
                self.timings.stop();
            }
        }

        self.update_players();

        // Handle commands before removing players just in case they ran a command before leaving
        self.handle_commands();

        self.remove_dc_players();
        self.remove_oob_players();
    }

    fn create_async_rt() -> Runtime {
        Runtime::new().unwrap()
    }

    fn generate_chunk(layers: i32, x: i32, z: i32) -> Chunk {
        let mut chunk = Chunk::empty(x, z, PLOT_SECTIONS);

        for ry in 0..layers {
            for rx in 0..16 {
                for rz in 0..16 {
                    let block_x = (x << 4) | rx;
                    let block_z = (z << 4) | rz;

                    let block = if block_x % PLOT_BLOCK_WIDTH == 0
                        || block_z % PLOT_BLOCK_WIDTH == 0
                        || (block_x + 1) % PLOT_BLOCK_WIDTH == 0
                        || (block_z + 1) % PLOT_BLOCK_WIDTH == 0
                    {
                        Block::StoneBricks {}
                    } else {
                        Block::Sandstone {}
                    };
                    chunk.set_block(rx as u32, ry as u32, rz as u32, block.get_id());
                }
            }
        }
        chunk
    }

    fn from_data(
        plot_data: PlotData,
        x: i32,
        z: i32,
        rx: BusReader<BroadcastMessage>,
        tx: Sender<Message>,
        priv_rx: Receiver<PrivMessage>,
        always_running: bool,
        fpga_scheduler: Arc<Mutex<FPGAScheduler>>,
    ) -> Plot {
        let chunk_x_offset = x << PLOT_SCALE;
        let chunk_z_offset = z << PLOT_SCALE;
        let chunks: Vec<Chunk> = plot_data
            .chunk_data
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                c.load(
                    chunk_x_offset + i as i32 / PLOT_WIDTH,
                    chunk_z_offset + i as i32 % PLOT_WIDTH,
                )
            })
            .collect();
        if chunks.len() != NUM_CHUNKS {
            error!("This plot has the wrong number of chunks!");
            let possible_scale = (chunks.len() as f64).sqrt().log2();
            error!("Note: it most likely came from a server running plot scale {}, this server is running a plot scale of {}", possible_scale, PLOT_SCALE);
        }
        let world = PlotWorld {
            x,
            z,
            chunks,
            to_be_ticked: plot_data.pending_ticks,
            packet_senders: Vec::new(),
        };
        let tps = plot_data.tps;
        let world_send_rate = plot_data.world_send_rate;
        let (back_tx, back_rx) = mpsc::channel();
        let backends = Backend::from_data((x,z), back_tx.clone(), fpga_scheduler.lock().unwrap().get_config());
        Plot {
            last_player_time: Instant::now(),
            last_update_time: Instant::now(),
            last_world_send_time: Instant::now(),
            lag_time: Duration::new(0, 0),
            sleep_time: sleep_time_for_tps(tps),
            last_nspt: None,
            message_receiver: rx,
            message_sender: tx,
            priv_message_receiver: priv_rx,
            players: Vec::new(),
            locked_players: HashSet::new(),
            running: true,
            auto_redpiler: CONFIG.auto_redpiler,
            tps,
            world_send_rate,
            always_running,
            backends: Arc::new(Mutex::new(backends)),
            active_backend: None,
            backend_rx: back_rx,
            backend_tx: back_tx,
            timings: TimingsMonitor::new(tps),
            owner: database::get_plot_owner(x, z).map(|s| s.parse::<HyphenatedUUID>().unwrap().0),
            async_rt: Plot::create_async_rt(),
            scoreboard: Scoreboard::new(),
            world:Arc::new(Mutex::new(world)),
            scheduler: fpga_scheduler, 
        }

    }

    fn load(
        x: i32,
        z: i32,
        rx: BusReader<BroadcastMessage>,
        tx: Sender<Message>,
        priv_rx: Receiver<PrivMessage>,
        always_running: bool,
        fpga_scheduler: Arc<Mutex<FPGAScheduler>>,
    ) -> Result<Plot, (Error, Sender<Message>)> {
        let plot_path = format!("./world/plots/p{},{}", x, z);
        Ok(if Path::new(&plot_path).exists() {
            match data::load_plot(plot_path) {
                Ok(data) => Plot::from_data(data, x, z, rx, tx, priv_rx, always_running, fpga_scheduler),
                Err(err) => {
                    return Result::Err((
                        err.context(format!("error loading plot {},{}", x, z)),
                        tx,
                    ))
                }
            }
        } else {
            Plot::from_data(data::empty_plot(), x, z, rx, tx, priv_rx, always_running, fpga_scheduler)
        })
    }

    fn save(&mut self) {
        {
            let world = &mut self.world.lock().unwrap();
            let chunk_data: Vec<ChunkData> =
                world.chunks.iter_mut().map(|c| ChunkData::new(c)).collect();
            let data = PlotData {
                tps: self.tps,
                world_send_rate: self.world_send_rate,
                chunk_data,
                pending_ticks: world.to_be_ticked.clone(),
            };
            data.save_to_file(format!("./world/plots/p{},{}", world.x, world.z))
                .unwrap();
        }

        self.reset_timings();
    }

    fn run(&mut self, initial_player: Option<Player>) {
        let _guard = self.async_rt.enter();

        if let Some(player) = initial_player {
            self.enter_plot(player);
        }

        while self.running {
            // Fast path, for super high RTPS
            if self.sleep_time <= Duration::from_millis(5) && !self.players.is_empty() {
                self.update();
                if self.tps != Tps::Unlimited {
                    thread::yield_now();
                }
                continue;
            }

            let before = Instant::now();
            self.update();
            let delta = Instant::now().duration_since(before);

            if delta < self.sleep_time {
                let sleep_time = self.sleep_time - delta;
                thread::sleep(sleep_time);
            } else {
                thread::yield_now();
            }
        }

        self.save();
    }

    /// This function is used in case of an error. It will try to send the player to spawn if this isn't already a spawn plot.
    fn send_player_away(plot_x: i32, plot_z: i32, player: &mut Player) {
        let (px, pz) = if plot_x == 0 && plot_z == 0 {
            // Can't send players to spawn if spawn crashed!
            Plot::get_center(1, 0)
        } else {
            Plot::get_center(0, 0)
        };
        player.teleport(PlayerPos::new(px, 64.0, pz));
    }

    pub fn load_and_run(
        x: i32,
        z: i32,
        rx: BusReader<BroadcastMessage>,
        tx: Sender<Message>,
        priv_rx: Receiver<PrivMessage>,
        always_running: bool,
        initial_player: Option<Player>,
        fpga_scheduler: Arc<Mutex<FPGAScheduler>>,
    ) {
        thread::Builder::new()
            .name(format!("p{},{}", x, z))
            .spawn(
                move || match Plot::load(x, z, rx, tx, priv_rx, always_running, fpga_scheduler) {
                    Ok(mut plot) => plot.run(initial_player),
                    Err((err, tx)) => {
                        if let Some(mut player) = initial_player {
                            player.send_error_message("There was an error loading that plot.");
                            Plot::send_player_away(x, z, &mut player);
                            tx.send(Message::PlayerLeavePlot(player)).unwrap();
                        }
                        tx.send(Message::PlotUnload(x, z)).unwrap();
                        panic!("{err:?}");
                    }
                },
            )
            .unwrap();
    }
}

impl Drop for Plot {
    fn drop(&mut self) {
        if !self.players.is_empty() {
            for player in &mut self.players {
                player.save(); // just in case

                let world = &self.world.lock().unwrap();
                Plot::send_player_away(world.x, world.z, player);

                player.send_error_message("The plot you were previously in has crashed!");
            }

            while !self.players.is_empty() {
                let uuid = self.players[0].uuid;
                let player = self.leave_plot(uuid);
                self.message_sender
                    .send(Message::PlayerLeavePlot(player))
                    .unwrap();
            }
        }
        {
            let world = &self.world.lock().unwrap();
            self.message_sender
                .send(Message::PlotUnload(world.x, world.z))
                .unwrap();
        }

        self.reset_backend();
        self.world.lock().unwrap()
            .chunks
            .iter_mut()
            .for_each(|chunk| chunk.compress());
        self.save();
    }
}

#[test]
fn chunk_save_and_load_test() {
    let mut chunk = Chunk::empty(1, 1, PLOT_SECTIONS);
    chunk.set_block(13, 63, 12, 332);
    chunk.set_block(13, 62, 12, 331);
    let chunk_data = ChunkData::new(&mut chunk);
    let loaded_chunk = chunk_data.load(1, 1);
    assert_eq!(loaded_chunk.get_block(13, 63, 12), 332);
    assert_eq!(loaded_chunk.get_block(13, 62, 12), 331);
    assert_eq!(loaded_chunk.get_block(13, 64, 12), 0);
}
