-- The node (worker_nodes.instance_id) whose run last moved the workflow; NULL until an attempt starts.
ALTER TABLE decision_delivery_attempts ADD COLUMN IF NOT EXISTS holder_instance_id TEXT;
