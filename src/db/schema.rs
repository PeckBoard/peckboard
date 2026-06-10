// Generated from migrations/00000000000001_initial/up.sql
// Equivalent to `diesel print-schema` output

diesel::table! {
    folders (id) {
        id -> Text,
        name -> Text,
        path -> Text,
        created_at -> Text,
    }
}

diesel::table! {
    sessions (id) {
        id -> Text,
        name -> Text,
        folder_id -> Text,
        model -> Nullable<Text>,
        effort -> Nullable<Text>,
        is_worker -> Bool,
        project_id -> Nullable<Text>,
        card_id -> Nullable<Text>,
        conversation_id -> Nullable<Text>,
        created_at -> Text,
        last_activity -> Text,
        is_expert -> Bool,
        expert_kind -> Nullable<Text>,
        knowledge_summary -> Nullable<Text>,
        knowledge_area -> Nullable<Text>,
        scope_path -> Nullable<Text>,
        is_permanent -> Bool,
        repeating_task_id -> Nullable<Text>,
    }
}

diesel::table! {
    repeating_tasks (id) {
        id -> Text,
        name -> Text,
        description -> Text,
        folder_id -> Text,
        prompt -> Text,
        schedule_kind -> Text,
        schedule_value -> Text,
        model -> Nullable<Text>,
        effort -> Nullable<Text>,
        enabled -> Bool,
        next_run_at -> Nullable<Text>,
        last_run_at -> Nullable<Text>,
        created_at -> Text,
        updated_at -> Text,
    }
}

diesel::table! {
    projects (id) {
        id -> Text,
        name -> Text,
        context -> Text,
        folder_id -> Text,
        worker_count -> Integer,
        status -> Text,
        workflow -> Text,
        model -> Nullable<Text>,
        effort -> Nullable<Text>,
        parallel_instructions -> Bool,
        auto_notify_changes -> Bool,
        worker_communication -> Bool,
        created_at -> Text,
        last_accessed_at -> Text,
        pause_reason -> Nullable<Text>,
    }
}

diesel::table! {
    cards (id) {
        id -> Text,
        project_id -> Text,
        title -> Text,
        description -> Text,
        step -> Text,
        priority -> Integer,
        workflow -> Text,
        model -> Nullable<Text>,
        effort -> Nullable<Text>,
        worker_session_id -> Nullable<Text>,
        last_worker_session_id -> Nullable<Text>,
        handoff_context -> Nullable<Text>,
        blocked -> Bool,
        block_reason -> Nullable<Text>,
        created_at -> Text,
        updated_at -> Text,
        completed_at -> Nullable<Text>,
    }
}

diesel::table! {
    card_dependencies (card_id, depends_on_card_id) {
        card_id -> Text,
        depends_on_card_id -> Text,
        created_at -> Text,
    }
}

diesel::table! {
    events (id) {
        id -> Text,
        session_id -> Text,
        seq -> Integer,
        ts -> BigInt,
        kind -> Text,
        data -> Text,
    }
}

diesel::table! {
    users (id) {
        id -> Text,
        username -> Text,
        email -> Nullable<Text>,
        password_hash -> Text,
        role -> Text,
        created_at -> Text,
        updated_at -> Text,
    }
}

diesel::table! {
    auth_sessions (id) {
        id -> Text,
        user_id -> Text,
        token_hash -> Text,
        created_at -> BigInt,
        expires_at -> BigInt,
        last_used_at -> Nullable<BigInt>,
        user_agent -> Nullable<Text>,
        ip_address -> Nullable<Text>,
    }
}

diesel::table! {
    push_subscriptions (endpoint) {
        endpoint -> Text,
        p256dh -> Text,
        auth_key -> Text,
        created_at -> Text,
    }
}

diesel::table! {
    queued_messages (session_id) {
        session_id -> Text,
        text -> Text,
        queued_at -> Text,
        model -> Nullable<Text>,
        effort -> Nullable<Text>,
    }
}

diesel::table! {
    announcements (id) {
        id -> Text,
        kind -> Text,
        title -> Text,
        message -> Text,
        detail -> Nullable<Text>,
        created_at -> Text,
    }
}

diesel::table! {
    user_tabs (user_id, item_type, item_id) {
        user_id -> Text,
        item_type -> Text,
        item_id -> Text,
        last_active -> Text,
    }
}

diesel::table! {
    todos (session_id, position) {
        session_id -> Text,
        position -> Integer,
        content -> Text,
        status -> Text,
        active_form -> Nullable<Text>,
        updated_at -> Text,
    }
}

diesel::joinable!(sessions -> folders (folder_id));
diesel::joinable!(sessions -> projects (project_id));
diesel::joinable!(sessions -> repeating_tasks (repeating_task_id));
diesel::joinable!(projects -> folders (folder_id));
diesel::joinable!(cards -> projects (project_id));
diesel::joinable!(events -> sessions (session_id));
diesel::joinable!(auth_sessions -> users (user_id));
diesel::joinable!(queued_messages -> sessions (session_id));
diesel::joinable!(todos -> sessions (session_id));
diesel::joinable!(repeating_tasks -> folders (folder_id));

diesel::joinable!(user_tabs -> users (user_id));

diesel::allow_tables_to_appear_in_same_query!(
    folders,
    sessions,
    projects,
    cards,
    card_dependencies,
    events,
    users,
    auth_sessions,
    push_subscriptions,
    queued_messages,
    announcements,
    user_tabs,
    todos,
    repeating_tasks,
);
