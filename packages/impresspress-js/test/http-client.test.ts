import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { HttpClient } from "../src/http-client";
import { ImpresspressError } from "../src/error";
import { fakeBlobResponse, fakeJsonResponse, hangingFetch } from "./fixtures";

/**
 * The single request path every service now goes through. These tests pin the
 * three behaviours the fold-in of `requestFormData`/`requestBlob` had to get
 * right: raw bodies are not JSON-encoded, a blob response is not pre-read as
 * text, and the 30 s default timeout can be switched off rather than silently
 * capping a large transfer.
 */
describe("HttpClient", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  describe("timeout", () => {
    it("aborts a JSON request at the 30 s default", async () => {
      const { fetchFn, captured } = hangingFetch();
      const http = new HttpClient({ url: "http://api.test", fetch: fetchFn as typeof fetch });

      const promise = http.get("/x");
      const assertion = expect(promise).rejects.toMatchObject({
        name: "ImpresspressError",
        code: "timeout",
      });
      await vi.advanceTimersByTimeAsync(30_000);
      await assertion;
      expect(captured.signal?.aborted).toBe(true);
    });

    it("honours an explicit per-request timeout", async () => {
      const { fetchFn } = hangingFetch();
      const http = new HttpClient({ url: "http://api.test", fetch: fetchFn as typeof fetch });

      const promise = http.get("/x", { timeout: 1_000 });
      const assertion = expect(promise).rejects.toMatchObject({ code: "timeout" });
      await vi.advanceTimersByTimeAsync(1_000);
      await assertion;
    });

    it("arms no timer at all for timeout: 0, and still honours the caller's signal", async () => {
      const { fetchFn, captured } = hangingFetch();
      const http = new HttpClient({ url: "http://api.test", fetch: fetchFn as typeof fetch });
      const controller = new AbortController();

      const promise = http.get("/big", { timeout: 0, signal: controller.signal });
      const assertion = expect(promise).rejects.toMatchObject({ code: "aborted" });

      // Ten minutes of a slow transfer: nothing must have aborted it.
      await vi.advanceTimersByTimeAsync(600_000);
      expect(captured.signal?.aborted).toBe(false);

      controller.abort();
      await assertion;
    });
  });

  describe("request bodies", () => {
    it("sends a FormData body raw, with no JSON encoding and no Content-Type", async () => {
      const fetchFn = vi.fn().mockResolvedValue(fakeJsonResponse({ uploaded: true }));
      const http = new HttpClient({ url: "http://api.test", fetch: fetchFn as unknown as typeof fetch });

      const form = new FormData();
      form.append("file", new Blob(["hi"]), "f.txt");
      await http.request("POST", "/upload", form);

      const [, init] = fetchFn.mock.calls[0];
      expect(init.body).toBe(form);
      expect(init.headers["Content-Type"]).toBeUndefined();
    });

    it("still JSON-encodes a plain object body", async () => {
      const fetchFn = vi.fn().mockResolvedValue(fakeJsonResponse({}));
      const http = new HttpClient({ url: "http://api.test", fetch: fetchFn as unknown as typeof fetch });

      await http.post("/x", { a: 1 });

      const [, init] = fetchFn.mock.calls[0];
      expect(JSON.parse(init.body)).toEqual({ a: 1 });
      expect(init.headers["Content-Type"]).toBe("application/json");
    });
  });

  describe("responseType", () => {
    it("returns the raw body for responseType 'blob' even when the server labels it JSON", async () => {
      const fetchFn = vi
        .fn()
        .mockResolvedValue(fakeBlobResponse('{"not":"an envelope"}', "application/json"));
      const http = new HttpClient({ url: "http://api.test", fetch: fetchFn as unknown as typeof fetch });

      const blob = await http.get("/objects/data.json", { responseType: "blob" });

      expect(blob).toBeInstanceOf(Blob);
      expect((blob as Blob).size).toBe('{"not":"an envelope"}'.length);
      expect((blob as Blob).type).toBe("application/json");
    });

    it("still decodes a JSON error body on a failed blob request", async () => {
      const fetchFn = vi
        .fn()
        .mockResolvedValue(fakeJsonResponse({ error: "NotFound", message: "no such object" }, 404));
      const http = new HttpClient({ url: "http://api.test", fetch: fetchFn as unknown as typeof fetch });

      await expect(http.get("/objects/gone", { responseType: "blob" })).rejects.toMatchObject({
        code: "NotFound",
        status: 404,
        message: "no such object",
      });
    });
  });

  it("rejects a method it cannot send with a machine-readable code", async () => {
    const fetchFn = vi.fn();
    const http = new HttpClient({ url: "http://api.test", fetch: fetchFn as unknown as typeof fetch });

    const error = await http
      .request("TRACE" as never, "/x")
      .then(() => null)
      .catch((e: unknown) => e);

    expect(error).toBeInstanceOf(ImpresspressError);
    expect((error as ImpresspressError).code).toBe("unsupported_method");
    expect(fetchFn).not.toHaveBeenCalled();
  });
});
