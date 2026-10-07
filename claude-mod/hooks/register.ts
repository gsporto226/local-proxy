import type { Register } from 'claude-code'

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    await $.command.register({
      name: 'proxy',
      description: 'Run the local-proxy CLI (e.g. /proxy account, /proxy logs)',
    })

    return next(e)
  })

  on('command.run', { command: 'proxy' }, async ($, e) => {
    // ponytail: whitespace split, no quoting; port exec::parse_args if quoted args show up
    const args = e.args.trim().split(/\s+/).filter(Boolean)
    try {
      const { exitCode, stdout, stderr } = await $.process.run(['local-proxy', ...args], {
        timeoutMs: 60_000,
      })
      const out = [stdout.trimEnd(), stderr.trimEnd()].filter(Boolean).join('\n')

      return { text: exitCode === 0 ? out || '(no output)' : `${out}\n(exit ${exitCode})` }
    } catch (err) {
      return { text: `local-proxy failed to run: ${String(err)}` }
    }
  })
}
