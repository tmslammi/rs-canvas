// Allow concurrent verification runs to own separate stateful test servers.
export function testPort(offset = 0): number {
  const base = Number(process.env.RS_CANVAS_TEST_PORT ?? 4173);
  if (!Number.isInteger(base) || base < 1024 || base > 65530) {
    throw new Error("RS_CANVAS_TEST_PORT must be an integer from 1024 to 65530");
  }
  return base + offset;
}

export function testBase(offset = 0): string {
  return `http://localhost:${testPort(offset)}`;
}
