import { Fragment } from "react";
import { Handle, Position, type NodeProps } from "@xyflow/react";
import { tieHeight, tieSlotTop } from "./campaignTieGeometry";

export type TieNodeData = { slots: number };

/** Inline because React Flow's unlayered stylesheet outranks the layered campaign styles. */
const slotHandle = (slot: number) => ({ top: tieSlotTop(slot), width: 4, height: 4, minWidth: 0, minHeight: 0, border: 0, background: "transparent" });

/** A group-graph tie handle: each strand enters and leaves at its own slot, so tied lines run side by side. */
export function CampaignTieNode({ data }: NodeProps) {
  const { slots } = data as TieNodeData;
  return <div className="campaign-tie" style={{ height: tieHeight(slots) }}>
    <i aria-hidden />
    {Array.from({ length: slots }, (_, slot) => <Fragment key={slot}>
      <Handle type="target" id={`in-${slot}`} position={Position.Left} isConnectable={false} style={slotHandle(slot)} />
      <Handle type="source" id={`out-${slot}`} position={Position.Right} isConnectable={false} style={slotHandle(slot)} />
    </Fragment>)}
  </div>;
}
