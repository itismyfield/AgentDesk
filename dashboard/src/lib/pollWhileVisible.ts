/** Runs `run` every `intervalMs` while the page is visible, and once as soon as a hidden page shows again.
 * Returns the cleanup for the effect that started it. */
export function pollWhileVisible(run: () => void, intervalMs: number): () => void {
  const onVisible = () => {
    if (!document.hidden) run();
  };
  const timer = window.setInterval(onVisible, intervalMs);
  document.addEventListener("visibilitychange", onVisible);
  return () => {
    window.clearInterval(timer);
    document.removeEventListener("visibilitychange", onVisible);
  };
}
