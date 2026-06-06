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
        default_workflow -> Nullable<Text>,
        model -> Nullable<Text>,
        effort -> Nullable<Text>,
        parallel_instructions -> Bool,
        created_at -> Text,
        last_accessed_at -> Text,
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
        workflow -> Nullable<Text>,
        model -> Nullable<Text>,
        effort -> Nullable<Text>,
        worker_session_id -> Nullable<Text>,
        last_worker_session_id -> Nullable<Text>,
        handoff_context -> Nullable<Text>,
        blocked -> Bool,
        block_reason -> Nullable<Text>,
        created_at -> Text,
        updated_at -> Text,
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

diesel::joinable!(sessions -> folders (folder_id));
diesel::joinable!(sessions -> projects (project_id));
diesel::joinable!(projects -> folders (folder_id));
diesel::joinable!(cards -> projects (project_id));
diesel::joinable!(events -> sessions (session_id));
diesel::joinable!(auth_sessions -> users (user_id));
diesel::joinable!(queued_messages -> sessions (session_id));

diesel::allow_tables_to_appear_in_same_query!(
    folders,
    sessions,
    projects,
    cards,
    events,
    users,
    auth_sessions,
    push_subscriptions,
    queued_messages,
    announcements,
);
