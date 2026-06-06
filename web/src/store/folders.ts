import { create } from 'zustand'
import type { Folder } from '../types/api'
import { authedFetch } from './auth'

interface FoldersState {
  folders: Folder[]
  fetchFolders: () => Promise<void>
  createFolder: (name: string, path: string) => Promise<Folder>
  deleteFolder: (id: string) => Promise<void>
}

export const useFoldersStore = create<FoldersState>((set) => ({
  folders: [],

  fetchFolders: async () => {
    const res = await authedFetch('/api/folders')
    if (res.ok) {
      const folders: Folder[] = await res.json()
      set({ folders })
    }
  },

  createFolder: async (name: string, path: string) => {
    const res = await authedFetch('/api/folders', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, path }),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to create folder' }))
      throw new Error(err.error || 'Failed to create folder')
    }
    const folder: Folder = await res.json()
    set((s) => ({ folders: [...s.folders, folder] }))
    return folder
  },

  deleteFolder: async (id: string) => {
    const res = await authedFetch(`/api/folders/${id}`, { method: 'DELETE' })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to delete folder' }))
      throw new Error(err.error || 'Failed to delete folder')
    }
    set((s) => ({ folders: s.folders.filter((f) => f.id !== id) }))
  },
}))
