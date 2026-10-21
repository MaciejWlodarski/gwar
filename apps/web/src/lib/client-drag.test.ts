import { describe, expect, it } from "vitest";
import { clientDragData, readClientDrag } from "./client-drag";

describe("client drag data", () => {
  it("carries the session id together with who held it", () => {
    expect(readClientDrag(clientDragData({ id: 4, uid: "ts:Ym9i" }))).toEqual({ id: 4, uid: "ts:Ym9i" });
  });

  it("drops anything without both", () => {
    for (const data of ["", "4", "null", "{}", '{"id":4}', '{"uid":"u"}', '{"id":0,"uid":"u"}', '{"id":"4","uid":"u"}', '{"id":4,"uid":""}', "{oops"]) {
      expect(readClientDrag(data)).toBeNull();
    }
  });
});
