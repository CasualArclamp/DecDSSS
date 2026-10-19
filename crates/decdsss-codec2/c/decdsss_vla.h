/* made by CasualArclamp: portable variable-length arrays for the vendored Codec2 core.
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
