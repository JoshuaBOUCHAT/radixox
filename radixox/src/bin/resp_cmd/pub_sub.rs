use radixox_lib::shared_byte::SharedByte;

use crate::{Frame, IOResult, SharedRegistry, utils::{ConnState, SubRegistry}};

pub(crate) async fn cmd_subscribe(
    args: &[SharedByte],
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
) -> IOResult<()> {
    for channel in args {
        let (_, _, count) = registry.borrow_mut().subscribe(conn_state, channel.clone());
        conn_state
            .send(
                Frame::Array(vec![
                    Frame::BulkString(SharedByte::from_str("subscribe")),
                    Frame::BulkString(channel.clone()),
                    Frame::Integer(count as i64),
                ]),
                registry,
            )
            .await?;
    }
    Ok(())
}

pub(crate) async fn cmd_unsubscribe(
    args: &[SharedByte],
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
) -> IOResult<()> {
    let frames = registry.borrow_mut().unsubscribe(conn_state, args);
    for frame in frames {
        conn_state.send(frame, registry).await?;
    }
    Ok(())
}

pub(crate) async fn cmd_publish(
    args: &[SharedByte],
    conn_state: &mut ConnState,
    registry: &SharedRegistry,
) -> IOResult<()> {
    let (response, to_flush) = registry.borrow_mut().publish_encode(args);
    conn_state.send(response, registry).await?;
    for sub_id in to_flush {
        SubRegistry::trigger_write(registry, sub_id);
    }
    Ok(())
}
