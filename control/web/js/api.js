async function parseJSON(res) {
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || res.statusText);
  }
  return res.json();
}

export const api = {
  stats: () => fetch("/runs/stats").then(parseJSON),
  listRuns: () => fetch("/runs").then(parseJSON),
  getRun: (id) => fetch(`/runs/${id}`).then(parseJSON),
  async getEvents(id) {
    const res = await fetch(`/runs/${id}/events`);
    if (!res.ok) {
      const text = await res.text();
      throw new Error(text || res.statusText);
    }
    const text = await res.text();
    if (!text.trim()) return [];
    return text
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
  },
  createRun: (body) =>
    fetch("/runs", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }).then(parseJSON),
  cancelRun: (id) =>
    fetch(`/runs/${id}/cancel`, { method: "POST" }).then(parseJSON),
  deleteRun: (id) =>
    fetch(`/runs/${id}`, { method: "DELETE" }).then(parseJSON),
  clearRuns: () =>
    fetch("/admin/runs/clear", { method: "POST" }).then(parseJSON),
  clearQueue: () =>
    fetch("/admin/queue/clear", { method: "POST" }).then(parseJSON),
};
