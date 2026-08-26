import assert from "node:assert/strict";
import test from "node:test";

import { decodeServiceOption, encodeServiceOption } from "../src/service-option.ts";

test("service option survives HTML-safe encoding and decoding", () => {
  const encoded = encodeServiceOption('商城\u0000“临时”<&', "tcp");

  assert.equal(encoded.includes("\u0000"), false);
  assert.equal(encoded.includes('"'), false);
  assert.equal(encoded.includes("<"), false);
  assert.equal(encoded.includes("&"), false);
  assert.deepEqual(decodeServiceOption(encoded), {
    service_id: '商城\u0000“临时”<&',
    protocol: "tcp",
  });
});

test("service option rejects values corrupted by HTML parsing", () => {
  assert.throws(() => decodeServiceOption("mall�tcp"), /请选择要授权的业务/);
  assert.throws(() => decodeServiceOption(""), /请选择要授权的业务/);
  assert.throws(
    () => decodeServiceOption(encodeURIComponent(JSON.stringify({ service_id: "mall", protocol: "http" }))),
    /请选择要授权的业务/,
  );
});
