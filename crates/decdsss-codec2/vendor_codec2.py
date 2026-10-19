"""Regenerates crates/decdsss-codec2/c from a Codec2 checkout.

usage: python vendor_codec2.py CODEC2_CHECKOUT GENERATE_CODEBOOK_EXE

CODEC2_CHECKOUT is a clone of github.com/drowe67/codec2 at commit
9f5e2deebcdf702ae9fd71785879891bc38d2f4e (the last before the 450 modes were
removed). GENERATE_CODEBOOK_EXE is src/generate_codebook.c built with any C
compiler (`cl generate_codebook.c`, or `gcc generate_codebook.c -lm`).

What it changes in the sources: variable-length arrays become VLA() (see
decdsss_vla.h: MSVC has none), the two-dimensional ones are flattened, and
`M` is an enum where it is the constant 4. Everything else is Codec2 as it was.
"""
import os, re, shutil, subprocess, sys

CODEC2_ROOT = sys.argv[1]
SRC = os.path.join(CODEC2_ROOT, 'src')
GEN = sys.argv[2]
DST = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'c')


CFILES = ["lpc", "nlp", "postfilter", "sine", "codec2", "codec2_fft", "kiss_fft", "kiss_fftr",
          "interp", "lsp", "mbest", "newamp1", "newamp2", "phase", "quantise", "pack", "golay23"]

if os.path.isdir(DST):
    shutil.rmtree(DST)
os.makedirs(DST)

# headers: closure of local includes
need_h = set()
todo = list(CFILES)
seen_c = set()
def includes(path):
    out = []
    for line in open(path, encoding='utf-8', errors='replace'):
        m = re.match(r'\s*#\s*include\s+"([^"]+)"', line)
        if m:
            out.append(m.group(1))
    return out
queue = [f + '.c' for f in CFILES]
while queue:
    f = queue.pop()
    p = os.path.join(SRC, f)
    if not os.path.exists(p):
        continue
    for inc in includes(p):
        base = inc
        if os.path.exists(os.path.join(SRC, base)) and base not in need_h:
            need_h.add(base)
            queue.append(base)
print('headers', sorted(need_h))

VLA_HEADER = '''/* made by CasualArclamp: portable variable-length arrays for the vendored Codec2 core.
   MSVC has no C99 VLAs, so the vendored sources declare them with VLA(); elsewhere
   it is the real thing, on MSVC a stack allocation of the same size. */
#ifndef DECDSSS_VLA_H
#define DECDSSS_VLA_H
#if defined(_MSC_VER)
#include <malloc.h>
#define VLA(type, name, n) type *name = (type *)_alloca(sizeof(type) * (size_t)(n))
#else
#define VLA(type, name, n) type name[n]
#endif
#endif
'''
open(os.path.join(DST, 'decdsss_vla.h'), 'w', newline='\n').write(VLA_HEADER)

CONST_RE = re.compile(r'^[A-Z0-9_+\-*/() ]*$')  # all-caps macro expressions are constants

def is_const(expr):
    e = expr.strip()
    if re.fullmatch(r'\d+', e):
        return True
    # identifiers that are ALL CAPS (macros) and operators only
    ids = re.findall(r'[A-Za-z_][A-Za-z0-9_]*', e)
    # Macros are ALL CAPS with at least two characters; K, M, N are variables here.
    return all((i.isupper() and len(i) > 1) or i == 'sizeof' for i in ids)

def split_decls(s):
    parts, depth, cur = [], 0, ''
    for ch in s:
        if ch in '[(':
            depth += 1
        elif ch in '])':
            depth -= 1
        if ch == ',' and depth == 0:
            parts.append(cur)
            cur = ''
        else:
            cur += ch
    parts.append(cur)
    return [p.strip() for p in parts]

DECL_RE = re.compile(r'^(\s*)(float|COMP|int|short|double|char)\s+(.*?);\s*(/\*.*\*/)?\s*$')

def patch_c(text, name):
    out = []
    changed = 0
    for line in text.split('\n'):
        m = DECL_RE.match(line)
        if m and '[' in m.group(3) and '=' not in m.group(3) and '(' not in m.group(3).replace('(order', '').replace('(K', ''):
            indent, typ, rest, cmt = m.groups()
            decls = split_decls(rest)
            # 2-D variable arrays are handled by hand below
            vl = []
            for d in decls:
                mm = re.fullmatch(r'(\w+)\s*\[(.+)\]', d)
                vl.append(mm)
            if any(mm and not is_const(mm.group(2)) and '][' not in d for mm, d in zip(vl, decls)) and not any('][' in d for d in decls):
                new = []
                plain = []
                for mm, d in zip(vl, decls):
                    if mm and not is_const(mm.group(2)):
                        new.append(f'{indent}VLA({typ}, {mm.group(1)}, {mm.group(2)});')
                    else:
                        plain.append(d)
                if plain:
                    new.insert(0, f'{indent}{typ} {", ".join(plain)};')
                if cmt:
                    new[-1] += ' ' + cmt
                out.extend(new)
                changed += 1
                continue
        out.append(line)
    return '\n'.join(out), changed

for f in CFILES:
    text = open(os.path.join(SRC, f + '.c'), encoding='utf-8', errors='replace').read()
    text, n = patch_c(text, f)
    if f == 'lpc':
        # The one two-dimensional VLA (levinson_durbin): flatten a[order+1][order+1].
        a = text.index('void levinson_durbin(')
        b = text.index('void inverse_filter(')
        body = text[a:b]
        body = body.replace('float a[order + 1][order + 1];', 'VLA(float, a, (order + 1) * (order + 1));')
        body = re.sub(r'a\[([^\]]+)\]\[([^\]]+)\]', lambda m: 'a[(%s) * (order + 1) + (%s)]' % (m.group(1), m.group(2)), body)
        text = text[:a] + body + text[b:]
        n += 1
    if f == 'codec2':
        # M is always 4 in the newamp decoders; as an enum it is a constant, so the
        # two-dimensional arrays that use it are not variable-length.
        old_m = '  int M = 4;\n  COMP HH[M]'
        c = text.count(old_m)
        assert c == 3, c
        text = text.replace(old_m, '  enum { M = 4 };\n  COMP HH[M]')
        n += c
    if n:
        # include the macro header after the first #include
        text = text.replace('#include', '#include "decdsss_vla.h"\n#include', 1)
    open(os.path.join(DST, f + '.c'), 'w', newline='\n', encoding='utf-8').write(text)
    print(f, 'patched decls:', n)

for h in sorted(need_h):
    shutil.copy(os.path.join(SRC, h), os.path.join(DST, h))

# codec2/version.h for <codec2/version.h>
os.makedirs(os.path.join(DST, 'codec2'))
open(os.path.join(DST, 'codec2', 'version.h'), 'w', newline='\n').write(
    '#define CODEC2_VERSION_MAJOR 1\n#define CODEC2_VERSION_MINOR 2\n#define CODEC2_VERSION_PATCH 0\n#define CODEC2_VERSION "1.2.0"\n')

# generated codebooks
cb = os.path.join(SRC, 'codebook')
def gen(name, table, files):
    r = subprocess.run([GEN, table] + [os.path.join(cb, f) for f in files], capture_output=True)
    assert r.returncode == 0, name
    hdr = ('/* Generated by Codec2\'s generate_codebook from its codebook/*.txt tables (see\n'
           '   crates/decdsss-codec2/README.md); LGPL-2.1, David Rowe. */\n').encode()
    open(os.path.join(DST, name), 'wb').write(hdr + r.stdout.replace(b'\r\n', b'\n'))
lsp = [f'lsp{i}.txt' for i in range(1, 11)]
dlsp = [f'dlsp{i}.txt' for i in range(1, 11)]
gen('codebook.c', 'lsp_cb', lsp)
gen('codebookd.c', 'lsp_cbd', dlsp)
gen('codebookjmv.c', 'lsp_cbjmv', ['lspjmv1.txt', 'lspjmv2.txt', 'lspjmv3.txt'])
gen('codebookge.c', 'ge_cb', ['gecb.txt'])
gen('codebooknewamp1.c', 'newamp1vq_cb', ['train_120_1.txt', 'train_120_2.txt'])
gen('codebooknewamp1_energy.c', 'newamp1_energy_cb', ['newamp1_energy_q.txt'])
gen('codebooknewamp2.c', 'newamp2vq_cb', ['codes_450.txt'])
gen('codebooknewamp2_energy.c', 'newamp2_energy_cb', ['newamp2_energy_q.txt'])
shutil.copy(os.path.join(CODEC2_ROOT, 'COPYING'), os.path.join(DST, 'COPYING'))
print('done')
