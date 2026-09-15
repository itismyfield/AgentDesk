from pathlib import Path
base=Path('src/services/discord/tmux_watcher')
p=base/'entry.rs';s=p.read_text();s=s.replace('use super::*;','use super::*;\nuse crate::services::cluster::stream_relay::RelayProducer;',1);s=s.replace('    crate::services::discord::jsonl_watcher::JsonlWatcher,','    Arc<crate::services::discord::jsonl_watcher::JsonlWatcher>,');p.write_text(s)
p=base/'turn_stream_collector.rs';s=p.read_text();old='capture_actor(shared, channel_id,';assert old in s;s=s.replace(old,'capture_actor(&shared, channel_id,');p.write_text(s)
p=base/'cancel_handoff.rs';s=p.read_text();old='''                    && self.turn.as_ref().and_then(|turn| turn.completion_actor.as_ref())
                        .and_then(std::sync::Weak::upgrade).is_some()
''';assert old in s;s=s.replace(old,'');p.write_text(s)
# Keep the same initial operations and ordering in the small entry module.
p=Path('src/services/discord/tmux_watcher.rs');s=p.read_text();a=s.index('    // #3041 P1-1:');b=s.index('    let (watcher_provider, watcher_channel_name)',a);block=s[a:b]
q=base/'entry.rs';q.write_text(q.read_text()+'''
/// Establish the watcher identity, producer and attach observation in order.
pub(super) fn start_watcher(
    channel_id: ChannelId, tmux_session_name: &str, initial_offset: u64,
) -> (u64, Arc<crate::services::cluster::relay_producer_registry::RelayProducerRegistry>, Option<RelayProducer>) {
'''+block.replace('entry::relay_producer(&tmux_session_name)','relay_producer(tmux_session_name)').replace('mut cached_relay_producer','cached_relay_producer')+'''    (watcher_instance_id, producer_registry, cached_relay_producer)
}
''')
p.write_text(s[:a]+'''    let (watcher_instance_id, producer_registry, mut cached_relay_producer) =
        entry::start_watcher(channel_id, &tmux_session_name, initial_offset);
'''+s[b:])
