import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { Markdown, safeHref } from "../src/lib/markdown";

function html(source: string): HTMLElement {
  return render(<Markdown source={source} />).container;
}

describe("untrusted markdown", () => {
  it("drops raw HTML, block and inline, instead of rendering or showing it", () => {
    const root = html('Hello <b onclick="x()">bold</b>\n\n<script>alert(1)</script>\n\n<img src=x onerror="alert(2)">\n\n<iframe src="https://evil.example"></iframe>');
    expect(root.querySelector("script, img, iframe, b")).toBeNull();
    expect(root.querySelectorAll("[onclick], [onerror]")).toHaveLength(0);
    expect(root.textContent).toContain("Hello");
    expect(root.textContent).not.toContain("alert");
  });

  it("links only absolute http(s) URLs, opened without referrer or opener and marked nofollow", () => {
    const root = html("[docs](https://docs.example/a?b=1) [plain](http://example.com) [js](javascript:alert(1)) [data](data:text/html,<b>x</b>) [rel](/api/v1/orgs/acme) [proto](//evil.example/x) [mail](mailto:a@b.c)");
    const links = [...root.querySelectorAll("a")];
    expect(links.map((a) => a.getAttribute("href"))).toEqual(["https://docs.example/a?b=1", "http://example.com"]);
    for (const a of links) {
      expect(a.getAttribute("rel")).toBe("noopener noreferrer nofollow");
      expect(a.getAttribute("target")).toBe("_blank");
    }
    // Refused links keep their text, without a link.
    expect(root.textContent).toContain("js");
    expect(root.textContent).toContain("rel");
  });

  it("shows images as links and never loads them", () => {
    const root = html("![screenshot of the bug](https://cdn.example/s.png) ![tracker](javascript:alert(1)) ![](http://img.example/x.gif)");
    expect(root.querySelector("img")).toBeNull();
    const links = [...root.querySelectorAll("a")];
    expect(links.map((a) => [a.getAttribute("href"), a.textContent, a.getAttribute("rel")])).toEqual([
      ["https://cdn.example/s.png", "[image: screenshot of the bug]", "noopener noreferrer nofollow"],
      ["http://img.example/x.gif", "[image: image]", "noopener noreferrer nofollow"],
    ]);
    expect(root.textContent).toContain("[tracker]");
  });

  it("autolinks bare URLs through the same rules and keeps code as text", () => {
    const root = html("See https://status.example/incident and `<b>not html</b>`\n\n```\n<script>x</script>\n```");
    expect([...root.querySelectorAll("a")].map((a) => a.getAttribute("href"))).toEqual(["https://status.example/incident"]);
    expect([...root.querySelectorAll("code")].map((c) => c.textContent)).toEqual(["<b>not html</b>", "<script>x</script>\n"]);
    expect(root.querySelector("b, script")).toBeNull();
  });

  it("accepts only http and https as link schemes", () => {
    expect(safeHref("https://a.example")).toBe("https://a.example");
    expect(safeHref("HTTP://A.EXAMPLE/x")).toBe("HTTP://A.EXAMPLE/x");
    for (const bad of ["javascript:alert(1)", " javascript:alert(1)", "data:text/html,x", "vbscript:x", "/relative", "//host/x", "file:///etc/passwd", "ftp://x"]) {
      expect(safeHref(bad)).toBeNull();
    }
  });
});
