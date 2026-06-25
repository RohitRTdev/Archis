#pragma once

typedef signed char      int8_t;
typedef short            int16_t;
typedef int              int32_t;
typedef long long        int64_t;

typedef unsigned char      uint8_t;
typedef unsigned short     uint16_t;
typedef unsigned int       uint32_t;
typedef unsigned long long uint64_t;
typedef uint64_t size_t;
typedef int64_t  ssize_t;

typedef uint64_t uintptr_t;
typedef int64_t  intptr_t;

typedef uint8_t boolean_t;

#define TRUE ((boolean_t)1)
#define FALSE ((boolean_t)0)

#ifndef NULL
#define NULL ((void*)0)
#endif