import { useEffect, useMemo } from 'react'
import { useSkillsStore } from '../../store/skills'
import type { PickerOption } from './SkillMultiPicker'

/** Picker options for "skills and groups": groups first, then installed
 *  skills. Values are `group:<id>` / `skill:<id>`. Loads the store once. */
export function useSkillOptions(): PickerOption[] {
  const skills = useSkillsStore((s) => s.skills)
  const groups = useSkillsStore((s) => s.groups)
  const skillsLoaded = useSkillsStore((s) => s.skillsLoaded)
  const groupsLoaded = useSkillsStore((s) => s.groupsLoaded)
  useEffect(() => {
    const st = useSkillsStore.getState()
    if (!skillsLoaded) void st.fetchSkills()
    if (!groupsLoaded) void st.fetchGroups()
  }, [skillsLoaded, groupsLoaded])
  return useMemo(
    () => [
      ...groups.map((g) => ({
        value: `group:${g.id}`,
        label: g.name || g.slug,
        description:
          g.description || `${g.skills.length} ${g.skills.length === 1 ? 'skill' : 'skills'}`,
        group: { id: 'groups', label: 'Groups' },
        searchText: `${g.slug} ${g.skills.join(' ')}`,
      })),
      ...skills.map((s) => ({
        value: `skill:${s.id}`,
        label: s.name,
        description: s.description,
        group: { id: 'skills', label: 'Skills' },
        searchText: s.tags.join(' '),
      })),
    ],
    [skills, groups],
  )
}

/** Split `group:<id>` / `skill:<id>` picker values into the API shape. */
export function splitSkillSelection(values: string[]): { skills: string[]; groups: string[] } {
  return {
    skills: values.filter((v) => v.startsWith('skill:')).map((v) => v.slice(6)),
    groups: values.filter((v) => v.startsWith('group:')).map((v) => v.slice(6)),
  }
}
