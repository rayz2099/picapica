/** why: 集成测试调本机二进制，不经 shell 拼命令，避免参数被二次解释。 */
export interface CommandResult {
  stdout: string;
  stderr: string;
}

export async function run(
  command: string,
  args: string[],
  timeoutMs = 300_000,
): Promise<CommandResult> {
  const proc = Bun.spawn([command, ...args], { stdout: "pipe", stderr: "pipe" });
  const stdout = new Response(proc.stdout).text();
  const stderr = new Response(proc.stderr).text();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      proc.kill();
      reject(new Error(`${command} ${args[0]} 超时`));
    }, timeoutMs);
  });

  try {
    const code = await Promise.race([proc.exited, timeout]);
    const result = { stdout: await stdout, stderr: await stderr };
    if (code !== 0) {
      const detail = [result.stdout, result.stderr].filter(Boolean).join("\n");
      throw new Error(`${command} ${args.join(" ")} 退出 ${code}\n${detail}`);
    }
    return result;
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}
