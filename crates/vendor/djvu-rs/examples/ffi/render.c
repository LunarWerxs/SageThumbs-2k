/* Example: render a DjVu page to PPM using the djvu-rs C API. */
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>

/* Minimal C declarations matching the Rust FFI exports. */
typedef struct djvu_doc_t djvu_doc_t;
typedef struct djvu_pixmap_t djvu_pixmap_t;

typedef struct {
    int32_t code;
    char   *message;
} djvu_error_t;

extern djvu_doc_t    *djvu_doc_open(const uint8_t *data, size_t len, djvu_error_t *err);
extern void           djvu_doc_free(djvu_doc_t *doc);
extern size_t         djvu_doc_page_count(const djvu_doc_t *doc);
extern uint32_t       djvu_page_width(const djvu_doc_t *doc, size_t page, djvu_error_t *err);
extern uint32_t       djvu_page_height(const djvu_doc_t *doc, size_t page, djvu_error_t *err);
extern uint32_t       djvu_page_dpi(const djvu_doc_t *doc, size_t page, djvu_error_t *err);
extern djvu_pixmap_t *djvu_page_render(const djvu_doc_t *doc, size_t page, float dpi, djvu_error_t *err);
extern void           djvu_pixmap_free(djvu_pixmap_t *pm);
extern uint32_t       djvu_pixmap_width(const djvu_pixmap_t *pm);
extern uint32_t       djvu_pixmap_height(const djvu_pixmap_t *pm);
extern const uint8_t *djvu_pixmap_data(const djvu_pixmap_t *pm);
extern size_t         djvu_pixmap_data_len(const djvu_pixmap_t *pm);
extern char          *djvu_page_text(const djvu_doc_t *doc, size_t page, djvu_error_t *err);
extern void           djvu_text_free(char *text);
extern void           djvu_error_free(djvu_error_t *err);

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "Usage: %s <file.djvu> [output.ppm]\n", argv[0]);
        return 1;
    }

    /* Read the DjVu file */
    FILE *f = fopen(argv[1], "rb");
    if (!f) { perror("fopen"); return 1; }
    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    fseek(f, 0, SEEK_SET);
    uint8_t *buf = malloc(sz);
    fread(buf, 1, sz, f);
    fclose(f);

    /* Open document */
    djvu_error_t err = {0};
    djvu_doc_t *doc = djvu_doc_open(buf, sz, &err);
    free(buf);
    if (!doc) {
        fprintf(stderr, "Error: %s\n", err.message);
        djvu_error_free(&err);
        return 1;
    }

    printf("Pages: %zu\n", djvu_doc_page_count(doc));

    /* Render page 0 at 150 DPI */
    djvu_pixmap_t *pm = djvu_page_render(doc, 0, 150.0f, &err);
    if (!pm) {
        fprintf(stderr, "Render error: %s\n", err.message);
        djvu_error_free(&err);
        djvu_doc_free(doc);
        return 1;
    }

    uint32_t w = djvu_pixmap_width(pm);
    uint32_t h = djvu_pixmap_height(pm);
    const uint8_t *pixels = djvu_pixmap_data(pm);

    printf("Rendered: %u x %u\n", w, h);

    /* Write PPM */
    const char *out = (argc > 2) ? argv[2] : "output.ppm";
    FILE *ppm = fopen(out, "wb");
    if (ppm) {
        fprintf(ppm, "P6\n%u %u\n255\n", w, h);
        for (uint32_t i = 0; i < w * h; i++) {
            fputc(pixels[i * 4 + 0], ppm);  /* R */
            fputc(pixels[i * 4 + 1], ppm);  /* G */
            fputc(pixels[i * 4 + 2], ppm);  /* B */
        }
        fclose(ppm);
        printf("Written: %s\n", out);
    }

    djvu_pixmap_free(pm);
    djvu_doc_free(doc);
    return 0;
}
