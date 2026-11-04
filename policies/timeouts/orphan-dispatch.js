module.exports = function attachOrphanDispatch(timeouts) {
  timeouts._section_K = function() {
    agentdesk.runtime.maintainOrphanDispatches();
  };
};
