module.exports = function attachActiveMonitor(timeouts) {
  timeouts._section_I = function() {
    agentdesk.timeouts.maintainStaleSessions();
  };
};
