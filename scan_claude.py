"""一次性扫描 ~/.claude/projects 下所有 transcript，统计真实 token 消耗。
去重：Claude Code 流式写入会让同一 message.id 出现多行，按 msg.id 去重。"""
import json, os, glob, collections

root = os.path.expanduser('~/.claude/projects')
seen = set()
by_model = collections.defaultdict(lambda: [0, 0, 0, 0])
by_month = collections.Counter()
ti = to = tcr = tcw = 0
n_dup = 0
files = glob.glob(root + '/**/*.jsonl', recursive=True)
first = last = None
sess = set()

for fp in files:
    try:
        with open(fp, encoding='utf-8', errors='replace') as f:
            for line in f:
                if '"usage"' not in line:
                    continue
                try:
                    e = json.loads(line)
                except Exception:
                    continue
                msg = e.get('message') or {}
                u = msg.get('usage') or {}
                if not u.get('output_tokens'):
                    continue
                key = msg.get('id') or e.get('uuid')
                if key and key in seen:
                    n_dup += 1
                    continue
                if key:
                    seen.add(key)
                ts = e.get('timestamp') or ''
                m = msg.get('model') or '?'
                i_ = u.get('input_tokens') or 0
                o_ = u.get('output_tokens') or 0
                cr = u.get('cache_read_input_tokens') or 0
                cw = u.get('cache_creation_input_tokens') or 0
                ti += i_; to += o_; tcr += cr; tcw += cw
                v = by_model[m]
                v[0] += i_; v[1] += o_; v[2] += cr; v[3] += cw
                by_month[ts[:7]] += i_ + o_ + cr + cw
                if ts:
                    if not first or ts[:10] < first:
                        first = ts[:10]
                    if not last or ts[:10] > last:
                        last = ts[:10]
                sid = e.get('sessionId')
                if sid:
                    sess.add(sid)
    except Exception:
        continue

print(f'文件: {len(files)} | 会话: {len(sess)}')
print(f'时间范围: {first} → {last}')
print(f'唯一消息: {len(seen)}（去重丢弃 {n_dup}）')
print(f'输入 {ti/1e6:.2f}M | 输出 {to/1e6:.2f}M | 缓存读 {tcr/1e6:.1f}M | 缓存写 {tcw/1e6:.1f}M')
print(f'总 token: {(ti+to+tcr+tcw)/1e9:.3f}B')
print()
print('分模型（总量 | in/out/缓存读/缓存写）:')
for m, v in sorted(by_model.items(), key=lambda kv: -sum(kv[1])):
    print(f'  {m}: {sum(v)/1e6:.1f}M | {v[0]/1e6:.2f}M / {v[1]/1e6:.2f}M / {v[2]/1e6:.1f}M / {v[3]/1e6:.1f}M')
print()
print('分月:')
for mo, v in sorted(by_month.items()):
    print(f'  {mo}: {v/1e6:.1f}M')
