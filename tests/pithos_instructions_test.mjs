import assert from "node:assert/strict";
import test from "node:test";
import register from "../pi-config/extensions/pithos-instructions.ts";

test("adds Pithos instructions without replacing existing prompt sections", () => {
  const handlers = new Map();
  register({ on: (name, handler) => handlers.set(name, handler) });
  assert.deepEqual([...handlers.keys()], ["before_agent_start"]);

  const sections = { preamble: "Pi defaults", project: "Project instructions" };
  const event = { systemPromptOptions: { sections } };
  const handler = handlers.get("before_agent_start");
  assert.equal(handler(event), undefined);
  assert.equal(sections.preamble, "Pi defaults");
  assert.equal(sections.project, "Project instructions");
  assert.match(sections.pithos, /# Communication Style/);
  assert.match(sections.pithos, /# Writing style/);
  assert.match(sections.pithos, /# How to work with me/);
  assert.match(sections.pithos, /\[Guessing\]/);
  assert.match(sections.pithos, /Always flag material security, data-loss, and compatibility/);

  const first = sections.pithos;
  handler(event);
  assert.equal(sections.pithos, first, "repeated runs must not duplicate instructions");
});
