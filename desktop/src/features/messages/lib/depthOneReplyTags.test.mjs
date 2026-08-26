import assert from "node:assert/strict";
import test from "node:test";

import { buildReplyTags, buildThreadReferenceTags } from "./threading.ts";

const CHANNEL = "channel";
const AUTHOR = "a".repeat(64);
const ROOT = "b".repeat(64);
const CHILD = "c".repeat(64);

test("replying to a child signs another direct reply to the root", () => {
  assert.deepEqual(buildReplyTags(CHANNEL, AUTHOR, CHILD, ROOT), [
    ["p", AUTHOR],
    ["h", CHANNEL],
    ["e", ROOT, "", "reply"],
  ]);
});

test("direct replies keep the same depth-one tag shape", () => {
  assert.deepEqual(buildReplyTags(CHANNEL, AUTHOR, ROOT, ROOT), [
    ["p", AUTHOR],
    ["h", CHANNEL],
    ["e", ROOT, "", "reply"],
  ]);
});

test("thread reference events also collapse child targets to the root", () => {
  assert.deepEqual(buildThreadReferenceTags(CHANNEL, CHILD, ROOT), [
    ["h", CHANNEL],
    ["e", ROOT, "", "reply"],
  ]);
});
