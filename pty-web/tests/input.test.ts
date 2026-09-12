import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { InputQueue, INPUT_CHUNK, INPUT_LIMIT } from "../src/input";
let sent: {data: number[]; id: string}[], queue: InputQueue;
let notice: ReturnType<typeof vi.fn>, recover: ReturnType<typeof vi.fn>;
const target = { attachment_id: "a", writer_epoch: 1 };
beforeEach(() => {
  vi.useFakeTimers(); sent = []; notice = vi.fn(); recover = vi.fn();
  queue = new InputQueue((_target, data, id) => sent.push({data, id}), notice, recover);
});
afterEach(() => vi.useRealTimers());
function ack(i: number, ok = true) { queue.submitted(sent[i].id, `r${i}`); queue.response(`r${i}`, ok); }
test("512KiB+ bytes are acknowledged chunk by chunk without loss", () => {
  const data = Uint8Array.from({length: 512 * 1024 + 137}, (_, i) => i % 251);
  queue.enqueue(target, data);
  expect(queue.response("other", true)).toBe(false);
  for (let i = 0; queue.bytes; i++) {
    expect(sent).toHaveLength(i + 1);
    expect(sent[i].data.length).toBeLessThanOrEqual(INPUT_CHUNK);
    ack(i);
  }
  expect(sent.flatMap(s => s.data)).toEqual(Array.from(data));
});
test("overflow rejects the whole input across tabs", () => {
  queue.enqueue(target, new Uint8Array(INPUT_LIMIT - 10));
  expect(queue.enqueue({...target, attachment_id: "b"}, new Uint8Array(11))).toBe(false);
  expect(queue.bytes).toBe(INPUT_LIMIT - 10);
  expect(notice).toHaveBeenCalledOnce();
  queue.enqueue({...target, attachment_id: "b"}, new Uint8Array(10));
  expect(queue.bytes).toBe(INPUT_LIMIT);
  expect(sent).toHaveLength(1);
});
test("error drops pending bytes without replay", () => {
  queue.enqueue(target, new Uint8Array(INPUT_CHUNK * 3)); ack(0, false);
  expect(queue.bytes).toBe(0); queue.response("r0", true);
  vi.advanceTimersByTime(20000); expect(sent).toHaveLength(1);
});
test("pending attachment invalidation preserves other tabs in FIFO order", () => {
  queue.enqueue(target, new Uint8Array([1, 2]));
  queue.enqueue({...target, attachment_id: "b"}, new Uint8Array([3, 4, 5]));
  queue.enqueue({...target, attachment_id: "c"}, new Uint8Array([6]));
  queue.enqueue(target, new Uint8Array([7, 8]));
  queue.invalidate("b"); queue.invalidate("missing");
  expect(queue.bytes).toBe(5); expect(sent).toHaveLength(1);
  ack(0); expect(queue.bytes).toBe(3);
  ack(1); expect(queue.bytes).toBe(2);
  ack(2); expect(queue.bytes).toBe(0);
  expect(sent.map(s => s.data)).toEqual([[1, 2], [6], [7, 8]]);
});
test.each([true, false])("invalidated flight settles (%s) without dropping or advancing other input", ok => {
  queue.enqueue(target, new Uint8Array(INPUT_CHUNK + 5));
  queue.enqueue({...target, attachment_id: "b"}, new Uint8Array([11, 12]));
  queue.enqueue(target, new Uint8Array([99]));
  queue.invalidate("a"); queue.invalidate("a");
  queue.enqueue({...target, writer_epoch: 2}, new Uint8Array([13]));
  expect(queue.bytes).toBe(INPUT_CHUNK + 3); expect(sent).toHaveLength(1);
  expect(queue.submitted(sent[0].id, "r0")).toBe(true);
  expect(queue.response("other", ok)).toBe(false);
  expect(queue.response("r0", ok, "partial write")).toBe(true);
  expect(queue.bytes).toBe(3); expect(sent[1].data).toEqual([11, 12]);
  expect(queue.response("r0", !ok)).toBe(false);
  expect(queue.bytes).toBe(3); expect(sent).toHaveLength(2);
  ack(1); expect(queue.bytes).toBe(1); expect(sent[2].data).toEqual([13]);
  ack(2); expect(queue.bytes).toBe(0);
  vi.advanceTimersByTime(20000); expect(recover).not.toHaveBeenCalled();
  expect(notice).toHaveBeenCalledTimes(ok ? 0 : 1);
});
test("tombstoned admitted bytes count toward the exact shared limit", () => {
  queue.enqueue(target, new Uint8Array(INPUT_CHUNK + 10));
  queue.invalidate("a");
  expect(queue.bytes).toBe(INPUT_CHUNK);
  expect(queue.enqueue({...target, attachment_id: "b"}, new Uint8Array(INPUT_LIMIT - INPUT_CHUNK))).toBe(true);
  expect(queue.bytes).toBe(INPUT_LIMIT);
  expect(queue.enqueue(target, new Uint8Array(1))).toBe(false);
  ack(0); expect(queue.bytes).toBe(INPUT_LIMIT - INPUT_CHUNK);
  ack(1); expect(queue.bytes).toBe(INPUT_LIMIT - 2 * INPUT_CHUNK);
  queue.clear(); expect(queue.bytes).toBe(0);
});
test.each(["write failed after 17 bytes", "", "   ", undefined])("errors preserve server details and warn about partial execution: %s", message => {
  queue.enqueue(target, new Uint8Array([1, 2]));
  queue.submitted(sent[0].id, "r0"); queue.response("r0", false, message);
  const warning = notice.mock.calls[0][0];
  if (message?.trim()) expect(warning).toContain(message);
  expect(warning).toContain("部分字节可能已到达远端终端");
  expect(warning).toContain("待发送输入已丢弃");
  expect(warning).toContain("请勿重复提交执行状态不确定的命令");
  expect(queue.bytes).toBe(0);
});
test("clear rejects stale submissions and replies even after another flight starts", () => {
  queue.enqueue(target, new Uint8Array([1]));
  queue.submitted(sent[0].id, "old"); queue.invalidate("a"); queue.clear();
  queue.enqueue(target, new Uint8Array([2, 3]));
  expect(queue.submitted(sent[0].id, "stale")).toBe(false);
  expect(queue.response("old", true)).toBe(false);
  expect(queue.response("old", false, "stale error")).toBe(false);
  expect(queue.response("stale", true)).toBe(false);
  expect(queue.bytes).toBe(2); expect(sent).toHaveLength(2);
  expect(notice).not.toHaveBeenCalled();
  ack(1); expect(queue.bytes).toBe(0);
});
test("watchdog discards pending bytes and recovers without replay", () => {
  queue.enqueue(target, new Uint8Array(INPUT_CHUNK * 3)); vi.advanceTimersByTime(15000);
  expect(queue.bytes).toBe(0); expect(recover).toHaveBeenCalledOnce();
  expect(notice).toHaveBeenCalledOnce(); ack(0); vi.advanceTimersByTime(30000);
  expect(sent).toHaveLength(1);
});

test("native rejection settles tombstone without clearing unrelated or replacement input", () => {
  queue.enqueue(target, new Uint8Array(INPUT_CHUNK + 3));
  queue.enqueue({...target, attachment_id: "b"}, new Uint8Array([2, 3]));
  queue.invalidate("a");
  queue.enqueue({...target, writer_epoch: 2}, new Uint8Array([4]));
  expect(queue.rejected("unrelated", "ignored")).toBe(false);
  expect(queue.bytes).toBe(INPUT_CHUNK + 3);
  expect(queue.rejected(sent[0].id, "native queue rejected input")).toBe(true);
  expect(queue.bytes).toBe(3);
  expect(sent).toHaveLength(2); expect(sent[1].data).toEqual([2, 3]);
  expect(notice).toHaveBeenCalledWith(expect.stringContaining("native queue rejected input"));
  expect(queue.rejected(sent[0].id, "stale")).toBe(false);
  ack(1); expect(sent[2].data).toEqual([4]); ack(2);
  expect(queue.bytes).toBe(0); expect(recover).not.toHaveBeenCalled();
});
test("active native rejection clears queued input and reset rejects stale native errors", () => {
  queue.enqueue(target, new Uint8Array(INPUT_CHUNK + 3));
  queue.enqueue({...target, attachment_id: "b"}, new Uint8Array([2]));
  const old = sent[0].id;
  expect(queue.rejected(old, "native rejection")).toBe(true);
  expect(queue.bytes).toBe(0); expect(sent).toHaveLength(1);
  expect(notice).toHaveBeenCalledWith(expect.stringContaining("部分字节可能已到达远端终端"));
  queue.enqueue(target, new Uint8Array([3]));
  const reset = sent[1].id;
  queue.clear(); queue.enqueue(target, new Uint8Array([4, 5]));
  expect(queue.rejected(old)).toBe(false); expect(queue.rejected(reset)).toBe(false);
  expect(queue.bytes).toBe(2); expect(notice).toHaveBeenCalledOnce();
  ack(2); vi.advanceTimersByTime(20000);
  expect(queue.bytes).toBe(0); expect(sent).toHaveLength(3);
});
