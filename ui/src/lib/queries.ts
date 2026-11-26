// Shared TanStack Query definitions. Keys include the namespace scope.
import { createQuery } from '@tanstack/svelte-query';
import { get } from './api/client';
import type { NamespaceInfo } from './api/types';

export const namespacesQuery = () =>
  createQuery(() => ({
    queryKey: ['namespaces'],
    queryFn: () => get<NamespaceInfo[]>('/namespaces'),
    staleTime: 10_000,
    refetchInterval: 30_000,
  }));
