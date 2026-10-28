-- External origins share admission storage without acquiring iMessage reply authority.
ALTER TABLE imsg_reply_obligations
    DROP CONSTRAINT imsg_reply_obligations_source_check,
    ADD CONSTRAINT imsg_reply_obligations_source_check
        CHECK (source IN ('imessage', 'external'));
