#ifndef FLOWSPLICE_TRAVEL_APPLE_H
#define FLOWSPLICE_TRAVEL_APPLE_H

#ifdef __cplusplus
extern "C" {
#endif
char *flowsplice_travel_begin_enrollment(
    const char *install_dir,
    const char *travel_id,
    const char *home_id,
    const char *selected_relay,
    const char *password
);
char *flowsplice_travel_enrollment_status(void);
char *flowsplice_travel_cancel_enrollment(void);
char *flowsplice_travel_start(const char *config_path, const char *password);
char *flowsplice_travel_stop(void);
char *flowsplice_travel_network_changed(void);
char *flowsplice_travel_status(void);
char *flowsplice_travel_catalog(void);
char *flowsplice_travel_upsert_mapping(const char *mapping_json);
char *flowsplice_travel_delete_mapping(
    const char *home_id,
    const char *service_id,
    const char *protocol_name
);
void flowsplice_travel_string_free(char *value);

#ifdef __cplusplus
}
#endif

#endif
