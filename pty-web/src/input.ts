export const INPUT_LIMIT = 4 * 1024 * 1024;
export const INPUT_CHUNK = 16384;
type Target = { attachment_id: string; writer_epoch: number };
type Pending = Target & { data: Uint8Array; offset: number };
/** One acknowledged bridge operation at a time, shared by all tabs of a Home. */
export class InputQueue {
  private pending: Pending[] = [];
  private flight?: { id: string; request?: string; size: number; attachment: string; discarded?: boolean };
  private timer?: ReturnType<typeof setTimeout>;
  bytes = 0;
  constructor(private send: (target: Target, data: number[], id: string) => void,
    private notice: (message: string) => void, private recover: () => void) {}
  enqueue(target: Target, data: Uint8Array) {
    if (data.length > INPUT_LIMIT - this.bytes) {
      this.notice("输入缓冲区已满，本次输入未发送。请等待后重试。");
      return false;
    }
    if (!data.length) return true;
    this.pending.push({ ...target, data, offset: 0 });
    this.bytes += data.length;
    this.pump();
    return true;
  }
  private pump() {
    if (this.flight || !this.pending.length) return;
    const item = this.pending[0];
    const size = Math.min(INPUT_CHUNK, item.data.length - item.offset);
    const id = crypto.randomUUID();
    this.flight = { id, size, attachment: item.attachment_id };
    this.timer = setTimeout(() => {
      this.clear();
      this.recover();
      this.notice("输入确认超时，待发送输入已丢弃；正在恢复连接，请勿重复提交可能已执行的命令。");
    }, 15000);
    try { this.send(item, Array.from(item.data.subarray(item.offset, item.offset + size)), id); }
    catch { this.clear(); this.notice("输入发送失败，待发送输入已丢弃。"); this.recover(); }
  }
  submitted(id: string, request: string) {
    if (this.flight?.id !== id) return false;
    this.flight.request = request;
    return true;
  }
  response(request: string, ok: boolean, message?: string) {
    if (!this.flight || this.flight.request !== request) return false;
    return this.settle(ok, message);
  }
  rejected(id: string, message?: string) {
    if (!this.flight || this.flight.id !== id) return false;
    return this.settle(false, message);
  }
  private settle(ok: boolean, message?: string) {
    const flight = this.flight!;
    const warning = "部分字节可能已到达远端终端，相关待发送输入已丢弃；请勿重复提交执行状态不确定的命令。";
    if (!ok && !flight.discarded) {
      this.clear();
      this.notice(message?.trim() ? `${message}；${warning}` : `输入失败，${warning}`);
      return true;
    }
    clearTimeout(this.timer);
    this.timer = undefined;
    this.bytes -= flight.size;
    this.flight = undefined;
    if (!flight.discarded) {
      const item = this.pending[0];
      item.offset += flight.size;
      if (item.offset === item.data.length) this.pending.shift();
    } else if (!ok) {
      this.notice(message?.trim() ? `${message}；${warning}` : `输入失败，${warning}`);
    }
    this.pump();
    return true;
  }
  invalidate(attachment: string) {
    const flight = this.flight;
    // Keep admitted bytes counted and correlated until their reply settles.
    // Removing their pending item must not let a reply advance another tab.
    if (flight?.attachment === attachment && !flight.discarded) {
      flight.discarded = true;
      this.bytes += flight.size;
    }
    this.pending = this.pending.filter(item => {
      if (item.attachment_id !== attachment) return true;
      this.bytes -= item.data.length - item.offset;
      return false;
    });
  }
  clear() {
    clearTimeout(this.timer);
    this.timer = undefined;
    this.flight = undefined;
    this.pending = [];
    this.bytes = 0;
  }
}
